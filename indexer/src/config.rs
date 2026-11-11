#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

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
    /// What an unattended recovery-drive update must egress as: the start of
    /// ipinfo.io's org line for the VM's public address, normally your ISP's
    /// AS number (`AS64500`). Unset, every unattended update refuses at its
    /// egress step — there is no built-in ISP (bd DAS-Backup-Manager-6pd3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_org: Option<String>,
    /// The login account `recovery-os-vm.sh provision` creates on a rebuilt
    /// recovery drive (autologin, NOPASSWD sudo, the only user sshd admits).
    /// Unset, provision refuses before it touches any drive — there is no
    /// built-in account name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_user: Option<String>,
    /// The directory of this installation's own filled-in recovery documents
    /// (`*.md`), which `recovery-os-vm.sh provision` carries to a rebuilt
    /// drive. Unset, the directory beside the config's own:
    /// `<config dir>-docs` (`/etc/das-backup-docs`). Never inside the
    /// config's directory, which is kept free of subdirectories so it can be
    /// versioned as flat files ([`local_docs_dir`]; bd
    /// DAS-Backup-Manager-5237.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_docs_dir: Option<String>,
}

fn default_recovery_os_max_age_days() -> u32 {
    60
}

/// `recovery_os.egress_org` goes into a shell script run in the recovery OS,
/// so it takes the same characters `recovery-os-vm.sh` accepts: ASCII letters,
/// digits and spaces, not starting with a space.
pub fn egress_org_is_valid(org: &str) -> bool {
    let mut chars = org.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == ' ')
}

/// `recovery_os.install_user` becomes a user name, a sudoers drop-in's file
/// name and an sshd `AllowUsers` word, so it takes what
/// `recovery-os-install.sh` accepts: a POSIX user name, `^[a-z_][a-z0-9_-]{0,31}$`
/// in ASCII, and never `root`.
pub fn install_user_is_valid(user: &str) -> bool {
    let mut chars = user.chars();
    user.len() <= 32
        && user != "root"
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl Default for RecoveryOs {
    fn default() -> Self {
        Self {
            max_age_days: default_recovery_os_max_age_days(),
            egress_org: None,
            install_user: None,
            local_docs_dir: None,
        }
    }
}

/// `recovery_os.local_docs_dir` is printed for a shell's `eval` and becomes
/// the installer's paths, so it is an absolute path of ASCII letters, digits,
/// `.`, `_`, `-` and `/`, with at least one component and none `.` or `..`.
pub fn local_docs_dir_is_valid(dir: &str) -> bool {
    let mut parts = dir.split('/').filter(|p| !p.is_empty()).peekable();
    dir.starts_with('/')
        && parts.peek().is_some()
        && dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        && parts.all(|p| p != "." && p != "..")
}

/// Where the installation's own recovery documents live for the config at
/// `config_path`: `recovery_os.local_docs_dir`, else the directory beside the
/// config's own, named `<that directory>-docs`. The one place the default is
/// derived: `dump-env` prints it for the scripts, `setup --upgrade` moves
/// [`legacy_local_docs_dir`] to it. A directory inside the config's own is
/// refused, as is a config with no directory to put one beside.
pub fn local_docs_dir(config: &Config, config_path: &Path) -> Result<PathBuf, String> {
    let conf_dir = config_path.parent().filter(|d| d.file_name().is_some()).ok_or_else(|| {
        format!(
            "{} has no directory beside which to keep the local recovery documents: set recovery_os.local_docs_dir",
            config_path.display()
        )
    })?;
    let dir = match &config.recovery_os.local_docs_dir {
        Some(d) => PathBuf::from(d),
        None => {
            let mut name = conf_dir.file_name().unwrap_or_default().to_os_string();
            name.push("-docs");
            conf_dir.with_file_name(name)
        }
    };
    if dir.starts_with(conf_dir) {
        return Err(format!(
            "recovery_os.local_docs_dir {} is inside {}, the config's directory, which must hold no subdirectory: choose a directory beside it",
            dir.display(),
            conf_dir.display()
        ));
    }
    Ok(dir)
}

/// Where versions before bd DAS-Backup-Manager-5237.6 kept the local
/// recovery documents: `docs/` in the config's own directory.
pub fn legacy_local_docs_dir(config_path: &Path) -> PathBuf {
    config_path.parent().unwrap_or(Path::new("/")).join("docs")
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
/// They were added so that a subdirectory of `/srv` (a VM-image directory, say)
/// can be granted in `allowed_roots` for an in-place restore (bd
/// DAS-Backup-Manager-tku): such a grant should be a *subdirectory* of `/srv`
/// and never `/srv` itself, and these entries make sure the served roots stay
/// denied even if someone widens the allow-list to the parent. Comparison is component-wise, so
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

/// Where a recovery boot may restore to, and nowhere else (spec D4).
pub const RECOVERY_ALLOWED_ROOTS: [&str; 2] = ["/mnt/restore", "/home/restore-test"];

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
    /// This configuration was derived for a recovery drive's own OS
    /// (`btrdasd config derive-recovery`): no sources, every target kept with
    /// the booted drive first, and restores allowed only under the recovery
    /// roots.
    #[serde(default)]
    pub recovery_drive: bool,
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
    /// A substring of your enclosure's USB product string (sysfs
    /// `/sys/bus/usb/devices/*/product`; for its disks, `device/model`).
    /// Empty (the default) is no enclosure filter: `backup-verify.sh` takes
    /// every USB disk, and `backup-run.sh` cannot tell which USB device is the
    /// enclosure, so it says so and records the link speed as unknown. No
    /// built-in product name — the default never names one vendor's enclosure.
    #[serde(default)]
    pub model_pattern: String,
    /// The USB link rate, in Mbit/s, the enclosure should negotiate:
    /// `backup-run.sh` fails its USB-link check below it. 10000 (USB 3.2
    /// Gen2) by default; a Gen1 enclosure sets 5000 (bd DAS-Backup-Manager-7qe).
    #[serde(default = "default_expected_link_mbit_s")]
    pub expected_link_mbit_s: u32,
    #[serde(default = "default_io_scheduler")]
    pub io_scheduler: String,
    #[serde(default)]
    pub mount_opts: String,
}

impl Default for Das {
    fn default() -> Self {
        Self {
            model_pattern: String::new(),
            expected_link_mbit_s: default_expected_link_mbit_s(),
            io_scheduler: default_io_scheduler(),
            mount_opts: String::new(),
        }
    }
}

fn default_expected_link_mbit_s() -> u32 {
    10000
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
    /// (e.g. `primary`, not the underlying BTRFS filesystem label
    /// `das-backup-primary`) — UUIDs are resolved from the existing
    /// `[[target]]` blocks at use time by joining on this field, never
    /// duplicated here. Empty (the default) means every configured
    /// `[[target]]`, in config order — read it through
    /// [`Config::scrub_target_labels`], never directly (bd
    /// DAS-Backup-Manager-dis3: a built-in label list scrubbed nothing on any
    /// install whose targets were named differently).
    #[serde(default)]
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
            targets: Vec::new(),
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Doctor {
    /// Additional glob patterns (`*`/`?` wildcards, matched case-sensitively
    /// against the full on-disk subvolume path) excluded from drift reporting,
    /// on top of the built-in exclusions. See `doctor::glob_match`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// The delivery check (bd DAS-Backup-Manager-sp78.7): every target must
    /// hold a snapshot of every pair `btrbk.conf` declares for it that is at
    /// most this many days old, or `btrdasd doctor` reports the pair as
    /// undelivered. At least 1; a target that cannot be read is never counted
    /// as fresh.
    #[serde(default = "default_doctor_max_snapshot_age_days")]
    pub max_snapshot_age_days: u32,
}

/// Three nights: a missed run or two does not raise it, a week-long silence does.
pub const DEFAULT_DOCTOR_MAX_SNAPSHOT_AGE_DAYS: u32 = 3;

fn default_doctor_max_snapshot_age_days() -> u32 {
    DEFAULT_DOCTOR_MAX_SNAPSHOT_AGE_DAYS
}

impl Default for Doctor {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            max_snapshot_age_days: default_doctor_max_snapshot_age_days(),
        }
    }
}

/// A subvolume within a source, with an optional snapshot name override.
/// Accepts bare strings ("@") or tables naming the subvolume and its options.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SubvolConfig {
    pub name: String,
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
    /// Snapshotted and sent only by the backup run's state pass, AFTER the
    /// indexer ran (the state subvolume, bd 9eq9): rendered into
    /// `btrbk-state.conf`, never into the main `btrbk.conf`, so the main run
    /// cannot take a pre-index snapshot that btrbk would then keep as the
    /// day's.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub after_index: bool,
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
                snapshot_name: Option<String>,
                #[serde(default)]
                adopted: Option<String>,
                #[serde(default)]
                retired: Option<String>,
                #[serde(default)]
                after_index: bool,
            },
        }

        match SubvolEntry::deserialize(deserializer)? {
            SubvolEntry::Simple(name) => Ok(SubvolConfig {
                name,
                ..Default::default()
            }),
            SubvolEntry::Full {
                name,
                snapshot_name,
                adopted,
                retired,
                after_index,
            } => Ok(SubvolConfig {
                name,
                snapshot_name,
                adopted,
                retired,
                after_index,
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
    /// The PARTUUID of a `role = "mirror"` drive's own ESP. When set, the
    /// recovery-OS reading mounts exactly that partition read-only and checks
    /// that every file its loader entries name is there (bd
    /// DAS-Backup-Manager-sp78.20); never discovered, and refused unless the
    /// drive itself lists it. Unset: the reading shows `ESP  not checked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub esp_partuuid: Option<String>,
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

    /// What identifies this target's drive whatever its label: `mount_uuid`
    /// when set, else the first serial (`serials`, then the legacy `serial`).
    /// `None` when there is neither. The coverage check carries a renamed
    /// target's pairs over by it.
    pub fn coverage_identity(&self) -> Option<String> {
        self.mount_uuid
            .iter()
            .chain(self.serials.iter())
            .chain(std::iter::once(&self.serial))
            .find(|s| !s.is_empty())
            .cloned()
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
    #[serde(default)]
    esp_partuuid: Option<String>,
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
            esp_partuuid: raw.esp_partuuid,
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
                version: crate::VERSION.to_string(),
                install_prefix: "/usr/local".into(),
                db_path: crate::db::DEFAULT_DB_PATH.into(),
                log_file: default_log_file(),
                growth_log: default_growth_log(),
                last_report: default_last_report(),
                btrbk_conf: default_btrbk_conf(),
                recovery_drive: false,
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

/// The first lines of a `config.toml` written fresh. Saves edit the file in
/// place, so what the operator writes in it stays (bd DAS-Backup-Manager-28il).
pub const FRESH_HEADER: &str = "# Written by btrdasd setup. Your edits and comments are kept when btrdasd saves this file.\n\
                                # After editing, run: sudo btrdasd setup --upgrade\n\n";

/// A warning for every key in a table this version reads that is not one of
/// that table's keys — a typo, most often, which the save keeps and nothing
/// reads (`target_lables` in a `[[source]]`) — with the nearest key the table
/// has when one is close. Keys of a top-level table this version does not
/// have are not warned about: a newer version's sections are expected there.
/// `Err` when `text` is not TOML.
pub fn unknown_key_warnings(text: &str) -> Result<Vec<String>, String> {
    let raw: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let schema = key_schema();
    let mut out = Vec::new();
    for (k, v) in &raw {
        if let Some(known) = schema.get(k) {
            unknown_keys_in(k, v, known, &mut out);
        }
    }
    Ok(out)
}

/// A GPT partition GUID as `lsblk` and `/dev/disk/by-partuuid` write it:
/// 8-4-4-4-12 hex digits, either case.
pub fn is_partuuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Every key this version reads, in the shape of a config: a config with
/// every optional key set, serialised, plus the legacy `serial` it reads and
/// never writes.
fn key_schema() -> toml::Table {
    let mut c = Config::default();
    c.recovery_os.egress_org = Some(String::new());
    c.recovery_os.install_user = Some(String::new());
    c.sources.push(Source {
        label: String::new(),
        volume: String::new(),
        subvolumes: vec![SubvolConfig {
            name: String::new(),
            snapshot_name: Some(String::new()),
            adopted: Some(String::new()),
            retired: Some(String::new()),
            after_index: true,
        }],
        device: String::new(),
        snapshot_dir: String::new(),
        target_subdirs: Vec::new(),
        target_labels: Vec::new(),
    });
    c.targets.push(Target {
        label: String::new(),
        serial: String::new(),
        serials: Vec::new(),
        mount_uuid: Some(String::new()),
        mount: String::new(),
        role: TargetRole::Primary,
        retention: Retention::default(),
        display_name: String::new(),
        esp_partuuid: Some(String::new()),
    });
    let mut schema: toml::Table = c
        .to_toml()
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default();
    if let Some(toml::Value::Array(targets)) = schema.get_mut("target")
        && let Some(toml::Value::Table(t)) = targets.first_mut()
    {
        t.insert("serial".into(), toml::Value::String(String::new()));
    }
    schema
}

fn unknown_keys_in(path: &str, v: &toml::Value, known: &toml::Value, out: &mut Vec<String>) {
    match (v, known) {
        (toml::Value::Table(vt), toml::Value::Table(kt)) => {
            for (k, sub) in vt {
                let at = format!("{path}.{k}");
                match kt.get(k) {
                    Some(ks) => unknown_keys_in(&at, sub, ks, out),
                    None => out.push(unknown_key_line(&at, k, kt)),
                }
            }
        }
        (toml::Value::Array(va), toml::Value::Array(ka)) => {
            if let Some(element) = ka.first().filter(|e| e.is_table()) {
                for (i, e) in va.iter().enumerate() {
                    unknown_keys_in(&format!("{path}[{i}]"), e, element, out);
                }
            }
        }
        _ => {}
    }
}

/// Keys an earlier version read and this one has removed: `(table, key, why)`,
/// `table` being the path of the table that held the key with its array
/// indices left out (`source.subvolumes`). Warned about by name, and only in
/// that table, so an old file is not told it holds a typo, and a key of the
/// same name elsewhere is not told it was removed (bd
/// DAS-Backup-Manager-s5jv.4).
const REMOVED_KEYS: &[(&str, &str, &str)] = &[(
    "source.subvolumes",
    // bd DAS-Backup-Manager-9ry (operator 2026-11-09): the renderer and
    // backup-run.sh never read it, so it kept nothing out of a backup.
    "manual_only",
    "manual_only was removed, and every backup run backs this subvolume up \u{2014} delete the key",
)];

fn unknown_key_line(path: &str, key: &str, known: &toml::Table) -> String {
    // `path` is `<table path>.<key>`, indices included (`source[0].subvolumes[1]`).
    let table = strip_indices(path.strip_suffix(key).unwrap_or(path).trim_end_matches('.'));
    if let Some((_, _, why)) = REMOVED_KEYS
        .iter()
        .find(|(t, k, _)| *t == table && *k == key)
    {
        return format!("{path} is no longer read: {why}");
    }
    let nearest = known
        .keys()
        .map(|k| (edit_distance(key, k), k))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d);
    match nearest {
        Some((_, k)) => format!("{path} is not a key this version reads (did you mean {k}?)"),
        None => format!("{path} is not a key this version reads"),
    }
}

/// `path` without its `[n]` array indices: `source[0].subvolumes[1]` is
/// `source.subvolumes`.
fn strip_indices(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut skipping = false;
    for c in path.chars() {
        match c {
            '[' => skipping = true,
            ']' => skipping = false,
            _ if !skipping => out.push(c),
            _ => {}
        }
    }
    out
}

/// Levenshtein distance: single-character insertions, deletions and
/// substitutions turning `a` into `b`.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diag = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let up = row[j + 1];
            row[j + 1] = if ca == *cb {
                diag
            } else {
                1 + diag.min(up).min(row[j])
            };
            diag = up;
        }
    }
    row[b.len()]
}

// ---------------------------------------------------------------------------
// Config saves as byte splices on config.toml (bd DAS-Backup-Manager-28il,
// DAS-Backup-Manager-5237.4)
// ---------------------------------------------------------------------------

/// The edits behind [`Config::save`]. The file is parsed with spans kept
/// (`toml_edit::Document`), `old` (what this binary knows of the file: its own
/// serialisation of what it loaded) is compared with `new` (what it saves), and
/// every difference becomes a splice on the ORIGINAL text: a value's bytes
/// replaced, a line or an element's text taken out, new text put in. Bytes
/// outside a splice are copied as they are, so nothing a save does not mean to
/// change can move or change. Rules: `Config::edited_into_with`.
///
/// Text is only ever looked into between the parser's spans (where TOML allows
/// nothing but spacing, commas, line ends and comments) or inside a decor span
/// (spacing and comments only): a comma or `#` inside a string is never seen.
mod splice {
    use std::ops::Range;
    use toml_edit::{Array, ArrayOfTables, InlineTable, Item, Table, TableLike, Value};

    /// The bytes `at` of the original text replaced by `text` (an insertion
    /// when `at` is empty, a deletion when `text` is).
    #[derive(Debug, Clone)]
    pub(super) struct Edit {
        at: Range<usize>,
        text: String,
    }

    /// Where a walk is in the config: `warn` names elements by their index
    /// in the file (`source[1].subvolumes[0]`), `schema` with `*` for every
    /// element (`source.*.subvolumes`), `header` is the header path a new
    /// table here would be written under (`source.subvolumes`).
    #[derive(Clone, Default)]
    struct At {
        warn: String,
        schema: Vec<String>,
        header: Vec<String>,
    }

    impl At {
        fn key(&self, k: &str) -> At {
            At {
                warn: match self.warn.is_empty() {
                    true => k.to_string(),
                    false => format!("{}.{k}", self.warn),
                },
                schema: self.schema.iter().cloned().chain([k.to_string()]).collect(),
                header: self.header.iter().cloned().chain([key_repr(k)]).collect(),
            }
        }

        fn element(&self, i: usize) -> At {
            At {
                warn: format!("{}[{i}]", self.warn),
                schema: self
                    .schema
                    .iter()
                    .cloned()
                    .chain(["*".to_string()])
                    .collect(),
                header: self.header.clone(),
            }
        }

        fn schema_with<'a>(&'a self, k: &'a str) -> Vec<&'a str> {
            self.schema.iter().map(String::as_str).chain([k]).collect()
        }
    }

    fn key_repr(k: &str) -> String {
        toml_edit::Key::new(k).to_string()
    }

    /// An entry of an inline table or array: its text `start..end` (an inline
    /// table's from its first key, dotted or not, to its value's end), its
    /// dotted key path (inline tables), the comma after it on its line, and
    /// the shape of its line.
    #[derive(Debug, Clone)]
    struct Entry {
        start: usize,
        end: usize,
        path: Vec<String>,
        comma: Option<usize>,
        /// The content end of its line, when nothing but a comma, spacing
        /// and a comment follows it there.
        last_on_line: Option<usize>,
        first_on_line: bool,
    }

    /// The pending edits of one inline table or array: entries removed,
    /// entries appended (their text), moves. Turned into splices once
    /// everything inside it was walked ([`Plan::finish_seq`]).
    struct Seq {
        span: Range<usize>,
        inline: bool,
        entries: Vec<Entry>,
        removed: Vec<bool>,
        appended: Vec<String>,
    }

    /// What a table's keys live in: the lines of a `[table]` body (the root,
    /// a `[header]` table, an `[[array]]` element), or an inline table.
    enum Cont<'a> {
        Body(&'a Table),
        Seq(Seq),
    }

    pub(super) struct Plan<'r> {
        raw: &'r str,
        edits: Vec<Edit>,
        pub(super) warnings: Vec<String>,
        /// Where every comment of the file starts (its `#`), sorted.
        comments: Vec<usize>,
        /// Where every table header of the file starts, sorted.
        headers: Vec<usize>,
        /// Set when an edit could not be placed: the save is refused.
        pub(super) unplaceable: Vec<String>,
        /// A section was already written at the end of the file.
        appended_at_end: bool,
    }

    /// The splices that turn `doc` (parsed from `raw`) from `old` into `new`.
    pub(super) fn plan<'r>(
        raw: &'r str,
        doc: &'r toml_edit::Document<&'r str>,
        old: &toml::Table,
        new: &toml::Table,
    ) -> Plan<'r> {
        let mut p = Plan {
            raw,
            edits: Vec::new(),
            warnings: Vec::new(),
            comments: Vec::new(),
            headers: Vec::new(),
            unplaceable: Vec::new(),
            appended_at_end: false,
        };
        p.index(doc);
        let root = doc.as_table();
        let mut cont = Cont::Body(root);
        p.merge_table(&mut cont, &[], root, old, new, &At::default());
        p
    }

    impl<'r> Plan<'r> {
        // -- reading the original text ------------------------------------

        fn bom(&self) -> usize {
            if self.raw.starts_with('\u{feff}') {
                3
            } else {
                0
            }
        }

        fn line_start(&self, p: usize) -> usize {
            self.raw[..p]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0)
                .max(self.bom().min(p))
        }

        /// The content end of the line holding `p` and the start of the next
        /// (both the text's end on a last line without a line end).
        fn line_end(&self, p: usize) -> (usize, usize) {
            match self.raw[p..].find('\n') {
                Some(i) => {
                    let nl = p + i;
                    let ce = if nl > p && self.raw.as_bytes()[nl - 1] == b'\r' {
                        nl - 1
                    } else {
                        nl
                    };
                    (ce, nl + 1)
                }
                None => (self.raw.len(), self.raw.len()),
            }
        }

        /// The line end `p`'s line has, else the one of the line before it,
        /// else `\n`.
        fn eol(&self, p: usize) -> &'static str {
            let (ce, next) = self.line_end(p);
            let has = |ce: usize, next: usize| match &self.raw[ce..next] {
                "\r\n" => Some("\r\n"),
                "\n" => Some("\n"),
                _ => None,
            };
            if let Some(e) = has(ce, next) {
                return e;
            }
            let ls = self.line_start(p);
            if ls > 0 {
                let (ce, next) = self.line_end(self.line_start(ls - 1));
                if let Some(e) = has(ce, next) {
                    return e;
                }
            }
            "\n"
        }

        fn indent(&self, p: usize) -> &'r str {
            let ls = self.line_start(p);
            let rest = &self.raw[ls..];
            let n = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            &self.raw[ls..ls + n]
        }

        fn blank(&self, a: usize, b: usize) -> bool {
            self.raw[a..b].bytes().all(|c| c == b' ' || c == b'\t')
        }

        fn has_comment(&self, r: Range<usize>) -> bool {
            let i = self.comments.partition_point(|&c| c < r.start);
            self.comments.get(i).is_some_and(|&c| c < r.end)
        }

        /// Is the line starting at `ls` empty but for spacing?
        fn blank_line(&self, ls: usize) -> bool {
            ls < self.raw.len() && {
                let (ce, _) = self.line_end(ls);
                self.blank(ls, ce)
            }
        }

        /// Is the line starting at `ls` only a comment?
        fn comment_line(&self, ls: usize) -> bool {
            ls < self.raw.len() && {
                let (ce, _) = self.line_end(ls);
                let t = self.raw[ls..ce].trim_start_matches([' ', '\t']);
                t.starts_with('#') && self.has_comment(ls..ce)
            }
        }

        fn index(&mut self, doc: &toml_edit::Document<&'r str>) {
            fn decor(raw: &str, d: &toml_edit::Decor, out: &mut Vec<usize>) {
                for s in [d.prefix(), d.suffix()].into_iter().flatten() {
                    raw_comments(raw, s, out);
                }
            }
            fn raw_comments(raw: &str, s: &toml_edit::RawString, out: &mut Vec<usize>) {
                if let Some(r) = s.span() {
                    let text = &raw[r.clone()];
                    let mut at = 0;
                    while let Some(i) = text[at..].find('#') {
                        let c = r.start + at + i;
                        out.push(c);
                        at += i + text[at + i..].find('\n').unwrap_or(text.len() - at - i);
                    }
                }
            }
            fn value(raw: &str, v: &Value, out: &mut Vec<usize>) {
                decor(raw, v.decor(), out);
                match v {
                    Value::Array(a) => {
                        raw_comments(raw, a.trailing(), out);
                        for e in a.iter() {
                            value(raw, e, out);
                        }
                    }
                    Value::InlineTable(t) => {
                        raw_comments(raw, t.trailing(), out);
                        table_like(raw, t, out, &mut Vec::new());
                    }
                    _ => {}
                }
            }
            fn table_like(
                raw: &str,
                t: &dyn TableLike,
                out: &mut Vec<usize>,
                headers: &mut Vec<usize>,
            ) {
                for (k, item) in t.iter() {
                    if let Some(key) = t.key(k) {
                        decor(raw, key.leaf_decor(), out);
                        decor(raw, key.dotted_decor(), out);
                    }
                    match item {
                        Item::Value(v) => value(raw, v, out),
                        Item::Table(t) => {
                            decor(raw, t.decor(), out);
                            if !t.is_dotted()
                                && let Some(s) = t.span()
                            {
                                headers.push(s.start);
                            }
                            table_like(raw, t, out, headers);
                        }
                        Item::ArrayOfTables(a) => {
                            for t in a.iter() {
                                decor(raw, t.decor(), out);
                                if let Some(s) = t.span() {
                                    headers.push(s.start);
                                }
                                table_like(raw, t, out, headers);
                            }
                        }
                        Item::None => {}
                    }
                }
            }
            let mut comments = Vec::new();
            let mut headers = Vec::new();
            table_like(self.raw, doc.as_table(), &mut comments, &mut headers);
            raw_comments(self.raw, doc.trailing(), &mut comments);
            comments.sort_unstable();
            comments.dedup();
            headers.sort_unstable();
            self.comments = comments;
            self.headers = headers;
        }

        fn edit(&mut self, at: Range<usize>, text: impl Into<String>) {
            self.edits.push(Edit {
                at,
                text: text.into(),
            });
        }

        fn refuse(&mut self, what: String) {
            self.unplaceable.push(what);
        }

        // -- the walk -------------------------------------------------------

        /// Apply `new` onto `logical` (a table read from `cont` under the
        /// dotted key `prefix`), `old` being what this binary knew of it.
        fn merge_table(
            &mut self,
            cont: &mut Cont<'r>,
            prefix: &[String],
            logical: &'r dyn TableLike,
            old: &toml::Table,
            new: &toml::Table,
            at: &At,
        ) {
            for (key, nv) in new {
                let ov = old.get(key);
                let here = at.key(key);
                let schema = at.schema_with(key);
                match logical.get(key) {
                    None => {
                        let write = match ov {
                            Some(ov) if ov == nv => pinned_part(&schema, nv),
                            Some(toml::Value::Table(old_t)) => match nv {
                                toml::Value::Table(nt) => {
                                    Some(toml::Value::Table(changed_part(&schema, old_t, nt)))
                                }
                                _ => Some(nv.clone()),
                            },
                            _ => Some(nv.clone()),
                        };
                        if let Some(v) = write {
                            self.insert(cont, prefix, key, &v, &here);
                        }
                    }
                    Some(item) => self.merge_item(cont, prefix, key, item, ov, nv, &here),
                }
            }
            for key in old.keys() {
                if !new.contains_key(key)
                    && let Some(item) = logical.get(key)
                {
                    self.remove(cont, prefix, key, item);
                }
            }
            // A legacy key this binary reads but never writes goes when the
            // key it feeds changes: kept, it is stale, and it may override the
            // change.
            for (legacy, fed) in LEGACY_KEYS {
                if old.get(*fed) != new.get(*fed)
                    && !new.contains_key(*legacy)
                    && let Some(item) = logical.get(legacy)
                {
                    self.remove(cont, prefix, legacy, item);
                }
            }
            // A subvolume entry's removed keys (`super::REMOVED_KEYS`) go
            // whether the entry changed or not: nothing reads them, and every
            // load warns about them until a save drops them (bd 9ry).
            let table = super::strip_indices(&at.warn);
            for (t, key, _) in super::REMOVED_KEYS {
                if *t == table
                    && !new.contains_key(*key)
                    && let Some(item) = logical.get(key)
                {
                    self.remove(cont, prefix, key, item);
                }
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn merge_item(
            &mut self,
            cont: &mut Cont<'r>,
            prefix: &[String],
            key: &str,
            item: &'r Item,
            ov: Option<&toml::Value>,
            nv: &toml::Value,
            at: &At,
        ) {
            let empty = toml::Table::new();
            let old_t = ov.and_then(toml::Value::as_table).unwrap_or(&empty);
            match (item, nv) {
                (Item::Table(t), toml::Value::Table(nt)) if t.is_dotted() => {
                    let p = extend(prefix, key);
                    self.merge_table(cont, &p, t, old_t, nt, at);
                }
                (Item::Table(t), toml::Value::Table(nt)) => {
                    let mut c = Cont::Body(t);
                    self.merge_table(&mut c, &[], t, old_t, nt, at);
                }
                (Item::Value(Value::InlineTable(t)), toml::Value::Table(nt)) if t.is_dotted() => {
                    let p = extend(prefix, key);
                    self.merge_table(cont, &p, t, old_t, nt, at);
                }
                (Item::Value(Value::InlineTable(t)), toml::Value::Table(nt)) => {
                    let mut c = Cont::Seq(self.inline_seq(t));
                    self.merge_table(&mut c, &[], t, old_t, nt, at);
                    if let Cont::Seq(s) = c {
                        self.finish_seq(s);
                    }
                }
                (Item::ArrayOfTables(aot), toml::Value::Array(na)) => {
                    let oa = ov
                        .and_then(toml::Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    self.merge_aot(cont, prefix, key, aot, oa, na, at);
                }
                (Item::Value(Value::Array(arr)), toml::Value::Array(na)) => {
                    let oa = ov
                        .and_then(toml::Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    if ov != Some(nv) || na.iter().any(toml::Value::is_table) {
                        self.merge_array(arr, oa, na, at);
                    }
                }
                (Item::Value(v), _) => {
                    if ov != Some(nv) {
                        self.set_value(v, nv);
                    }
                }
                _ => {
                    if ov != Some(nv) {
                        self.refuse(at.warn.clone());
                    }
                }
            }
        }

        fn set_value(&mut self, v: &Value, nv: &toml::Value) {
            match v.span() {
                Some(s) => self.edit(s, fresh_value(nv).to_string()),
                None => self.refuse("a value without a place in the file".into()),
            }
        }

        // -- arrays of tables -------------------------------------------------

        #[allow(clippy::too_many_arguments)]
        fn merge_aot(
            &mut self,
            cont: &mut Cont<'r>,
            prefix: &[String],
            key: &str,
            aot: &'r ArrayOfTables,
            oa: &[toml::Value],
            na: &[toml::Value],
            at: &At,
        ) {
            let picks = pick(oa, na);
            let elems: Vec<&'r Table> = aot.iter().collect();
            if elems.len() != oa.len() {
                self.refuse(at.warn.clone());
                return;
            }
            let mut kept = vec![false; oa.len()];
            for i in picks.iter().flatten() {
                kept[*i] = true;
            }
            // Dropped elements first: their warnings name file positions, the
            // outer before the inner.
            for (i, t) in elems.iter().enumerate() {
                if kept[i] {
                    continue;
                }
                let ranges = self.subtree_ranges(t);
                let comment = ranges.iter().any(|r| self.has_comment(r.clone()));
                if let Some(old_t) = oa[i].as_table() {
                    self.warnings
                        .extend(dropped_warning(&at.warn, i, *t, old_t, comment));
                }
                for r in ranges {
                    self.delete_lines(r);
                }
            }
            // Kept elements, each edited on its own; in a new order, each
            // one's edited text moves to the place the element now has.
            let order: Vec<usize> = picks.iter().flatten().copied().collect();
            let moved = !order.is_sorted();
            let mut own: Vec<(usize, Vec<Edit>)> = Vec::new();
            for (j, ne) in na.iter().enumerate() {
                let (Some(i), Some(nt)) = (picks[j], ne.as_table()) else {
                    continue;
                };
                let old_t = oa[i].as_table().cloned().unwrap_or_default();
                let saved = std::mem::take(&mut self.edits);
                let mut c = Cont::Body(elems[i]);
                self.merge_table(&mut c, &[], elems[i], &old_t, nt, &at.element(i));
                let mine = std::mem::replace(&mut self.edits, saved);
                own.push((i, mine));
            }
            if moved {
                // Slot k (the k-th kept element in the file) takes the k-th
                // element in the new order.
                let mut slots: Vec<usize> = order.clone();
                slots.sort_unstable();
                let ranges: Option<Vec<Range<usize>>> =
                    slots.iter().map(|&i| self.move_range(elems[i])).collect();
                let Some(ranges) = ranges else {
                    self.refuse(format!("{} (reordered)", at.warn));
                    return;
                };
                for (k, (src, mine)) in own.iter().enumerate() {
                    let from = ranges[slots.iter().position(|s| s == src).unwrap_or(k)].clone();
                    match apply_within(self.raw, &from, mine) {
                        Ok(t) => self.edit(ranges[k].clone(), t),
                        Err(e) => self.refuse(e),
                    }
                }
            } else {
                for (_, mine) in own {
                    self.edits.extend(mine);
                }
            }
            // New elements: written after the last element in the file.
            let fresh: Vec<&toml::Table> = na
                .iter()
                .zip(&picks)
                .filter(|(_, p)| p.is_none())
                .filter_map(|(ne, _)| ne.as_table())
                .collect();
            if !fresh.is_empty()
                && let Some(last) = elems.last()
            {
                let end = self.subtree_end(last);
                let (_, pos) = self.line_end(end);
                let eol = self.eol(end);
                let gap = self.blank_line_above(last);
                let mut text = String::new();
                if pos == self.raw.len() && !self.raw[..pos].ends_with('\n') {
                    text.push_str(eol);
                }
                for nt in fresh {
                    if gap {
                        text.push_str(eol);
                    }
                    text.push_str(&section(&at.header, true, nt, eol));
                }
                self.edit(pos..pos, text);
            }
            // Emptied: a required array (a source's `subvolumes`) is written
            // as `key = []`; at the top level an absent array of tables reads
            // as empty.
            if na.is_empty() && !oa.is_empty() && at.header.len() > 1 {
                self.insert(cont, prefix, key, &toml::Value::Array(Vec::new()), at);
            }
        }

        /// Was the line above `t`'s header blank?
        fn blank_line_above(&self, t: &Table) -> bool {
            let Some(h) = t.span() else { return false };
            let ls = self.line_start(h.start);
            ls > 0 && self.blank_line(self.line_start(ls - 1))
        }

        /// Every span inside `t` and its sub-tables: (header starts, max end).
        fn subtree(&self, t: &Table, headers: &mut Vec<usize>, end: &mut usize) {
            if let Some(s) = t.span() {
                if !t.is_dotted() {
                    headers.push(s.start);
                }
                *end = (*end).max(s.end);
            }
            for (_, v) in t.get_values() {
                if let Some(s) = v.span() {
                    *end = (*end).max(s.end);
                }
            }
            for (_, item) in t.iter() {
                match item {
                    Item::Table(c) => self.subtree(c, headers, end),
                    Item::ArrayOfTables(a) => {
                        for c in a.iter() {
                            self.subtree(c, headers, end);
                        }
                    }
                    _ => {}
                }
            }
        }

        fn subtree_end(&self, t: &Table) -> usize {
            let mut end = 0;
            self.subtree(t, &mut Vec::new(), &mut end);
            end
        }

        /// The text of `t` and every table under it: its header's line to the
        /// line of the last thing under it, in one piece when no other table
        /// starts inside it, else table by table.
        fn subtree_ranges(&self, t: &Table) -> Vec<Range<usize>> {
            let mut headers = Vec::new();
            let mut end = 0;
            self.subtree(t, &mut headers, &mut end);
            let Some(start) = headers.iter().min().copied() else {
                return Vec::new();
            };
            let whole = self.line_start(start)..self.line_end(end).1;
            let foreign = self
                .headers
                .iter()
                .any(|h| whole.contains(h) && !headers.contains(h));
            if !foreign {
                return vec![whole];
            }
            let mut out = Vec::new();
            self.each_table(t, &mut out);
            out
        }

        fn each_table(&self, t: &Table, out: &mut Vec<Range<usize>>) {
            if let Some(s) = t.span().filter(|_| !t.is_dotted()) {
                let end = t
                    .get_values()
                    .iter()
                    .filter_map(|(_, v)| v.span())
                    .map(|r| r.end)
                    .max()
                    .unwrap_or(s.end)
                    .max(s.end);
                out.push(self.line_start(s.start)..self.line_end(end).1);
            }
            for (_, item) in t.iter() {
                match item {
                    Item::Table(c) => self.each_table(c, out),
                    Item::ArrayOfTables(a) => a.iter().for_each(|c| self.each_table(c, out)),
                    _ => {}
                }
            }
        }

        /// What moves with an element kept in a new order: the comment lines
        /// above its header (after a blank line), its own text, and the
        /// comment lines touching its last line.
        fn move_range(&self, t: &Table) -> Option<Range<usize>> {
            let ranges = self.subtree_ranges(t);
            let [whole] = ranges.as_slice() else {
                return None;
            };
            let mut start = whole.start;
            let mut top = start;
            while top > 0 {
                let ls = self.line_start(top - 1);
                if !self.comment_line(ls) {
                    break;
                }
                top = ls;
            }
            if top < start && (top == 0 || self.blank_line(self.line_start(top - 1))) {
                start = top;
            }
            let mut end = whole.end;
            while self.comment_line(end) {
                end = self.line_end(end).1;
            }
            Some(start..end)
        }

        // -- inline tables and arrays ----------------------------------------

        fn entry(&self, start: usize, end: usize, path: Vec<String>) -> Entry {
            let b = self.raw.as_bytes();
            let mut p = end;
            while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
                p += 1;
            }
            let comma = (p < b.len() && b[p] == b',').then_some(p);
            let mut q = comma.map(|c| c + 1).unwrap_or(end);
            while q < b.len() && (b[q] == b' ' || b[q] == b'\t') {
                q += 1;
            }
            let last_on_line =
                (q >= b.len() || matches!(b[q], b'#' | b'\r' | b'\n')).then(|| self.line_end(q).0);
            let first_on_line = self.blank(self.line_start(start), start);
            Entry {
                start,
                end,
                path,
                comma,
                last_on_line,
                first_on_line,
            }
        }

        fn inline_seq(&self, t: &InlineTable) -> Seq {
            let mut entries: Vec<Entry> = t
                .get_values()
                .into_iter()
                .filter_map(|(keys, v)| {
                    let start = keys.first()?.span()?.start;
                    let end = v.span()?.end;
                    let path = keys.iter().map(|k| k.get().to_string()).collect();
                    Some(self.entry(start, end, path))
                })
                .collect();
            entries.sort_by_key(|e| e.start);
            Seq {
                span: t.span().unwrap_or_default(),
                inline: true,
                removed: vec![false; entries.len()],
                entries,
                appended: Vec::new(),
            }
        }

        fn array_seq(&self, a: &Array) -> Seq {
            let entries: Vec<Entry> = a
                .iter()
                .filter_map(|v| v.span())
                .map(|s| self.entry(s.start, s.end, Vec::new()))
                .collect();
            Seq {
                span: a.span().unwrap_or_default(),
                inline: false,
                removed: vec![false; entries.len()],
                entries,
                appended: Vec::new(),
            }
        }

        fn merge_array(&mut self, arr: &'r Array, oa: &[toml::Value], na: &[toml::Value], at: &At) {
            let mut seq = self.array_seq(arr);
            let vals: Vec<&'r Value> = arr.iter().collect();
            if seq.entries.len() != vals.len() || vals.len() != oa.len() {
                self.refuse(at.warn.clone());
                return;
            }
            let picks = pick(oa, na);
            let mut kept = vec![false; oa.len()];
            for i in picks.iter().flatten() {
                kept[*i] = true;
            }
            for (i, v) in vals.iter().enumerate() {
                if kept[i] {
                    continue;
                }
                seq.removed[i] = true;
                // An entry (a table to this binary), inline or a bare string.
                if let Some(old_t) = oa[i].as_table() {
                    let e = &seq.entries[i];
                    let reach = e.last_on_line.unwrap_or(e.end);
                    let comment = self.has_comment(e.start..reach.max(e.end));
                    let bare = InlineTable::new();
                    let t = v.as_inline_table().unwrap_or(&bare);
                    self.warnings
                        .extend(dropped_warning(&at.warn, i, t, old_t, comment));
                }
            }
            let order: Vec<usize> = picks.iter().flatten().copied().collect();
            let moved = !order.is_sorted();
            let mut own: Vec<(usize, Vec<Edit>)> = Vec::new();
            for (j, ne) in na.iter().enumerate() {
                let Some(i) = picks[j] else { continue };
                let saved = std::mem::take(&mut self.edits);
                self.merge_element(vals[i], &oa[i], ne, &at.element(i));
                let mine = std::mem::replace(&mut self.edits, saved);
                own.push((i, mine));
            }
            if moved {
                let mut slots = order.clone();
                slots.sort_unstable();
                for (k, (src, mine)) in own.iter().enumerate() {
                    let slot = &seq.entries[slots[k]];
                    let from = &seq.entries[*src];
                    let value = match apply_within(self.raw, &(from.start..from.end), mine) {
                        Ok(t) => t,
                        Err(e) => {
                            self.refuse(e);
                            continue;
                        }
                    };
                    let (to, text) = match slot.last_on_line {
                        Some(ce) => {
                            let tail = match from.last_on_line {
                                Some(fce) => {
                                    let after = from.comma.map(|c| c + 1).unwrap_or(from.end);
                                    self.raw[after..fce].to_string()
                                }
                                None => String::new(),
                            };
                            let comma = if slot.comma.is_some() { "," } else { "" };
                            (slot.start..ce, format!("{value}{comma}{tail}"))
                        }
                        None => (slot.start..slot.end, value),
                    };
                    self.edit(to, text);
                }
            } else {
                for (_, mine) in own {
                    self.edits.extend(mine);
                }
            }
            for (ne, p) in na.iter().zip(&picks) {
                if p.is_none() {
                    seq.appended.push(fresh_value(ne).to_string());
                }
            }
            self.finish_seq(seq);
        }

        /// A kept element of an inline array: an inline table edited in place;
        /// a bare string (a subvolume entry written `"@"`) kept while unchanged
        /// and, when it changes, written as a table holding its name and only
        /// the values that differ from what the bare string meant.
        fn merge_element(&mut self, v: &'r Value, ov: &toml::Value, nv: &toml::Value, at: &At) {
            match (v, ov, nv) {
                (Value::InlineTable(t), toml::Value::Table(old_t), toml::Value::Table(nt)) => {
                    let mut c = Cont::Seq(self.inline_seq(t));
                    self.merge_table(&mut c, &[], t, old_t, nt, at);
                    if let Cont::Seq(s) = c {
                        self.finish_seq(s);
                    }
                }
                (Value::String(_), toml::Value::Table(old_t), toml::Value::Table(nt)) => {
                    if old_t != nt {
                        let changed: toml::Table = nt
                            .iter()
                            .filter(|(k, val)| *k == "name" || old_t.get(*k) != Some(*val))
                            .map(|(k, val)| (k.clone(), val.clone()))
                            .collect();
                        self.set_value(v, &toml::Value::Table(changed));
                    }
                }
                _ => {
                    if ov != nv {
                        self.set_value(v, nv);
                    }
                }
            }
        }

        /// The splices for `seq`'s removed and appended entries.
        fn finish_seq(&mut self, seq: Seq) {
            let Seq {
                span,
                inline,
                entries,
                removed,
                appended,
            } = seq;
            if span.is_empty() {
                if removed.iter().any(|r| *r) || !appended.is_empty() {
                    self.refuse("an inline table or array without a place in the file".into());
                }
                return;
            }
            let kept: Vec<usize> = (0..entries.len()).filter(|&i| !removed[i]).collect();
            let mut first_removal: Option<Range<usize>> = None;
            for i in (0..entries.len()).filter(|&i| removed[i]) {
                let e = &entries[i];
                let before = kept.iter().rev().find(|&&k| k < i).map(|&k| &entries[k]);
                let after_kept = kept.iter().any(|&k| k > i);
                let prev_tok = {
                    let b = self.raw.as_bytes();
                    let mut p = e.start;
                    while p > 0 && (b[p - 1] == b' ' || b[p - 1] == b'\t') {
                        p -= 1;
                    }
                    p
                };
                let after_comma = e.comma.map(|c| {
                    let b = self.raw.as_bytes();
                    let mut p = c + 1;
                    while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
                        p += 1;
                    }
                    p
                });
                // The comma of the kept entry before, when on this line (or
                // only spacing and a line end between).
                let sep = before.and_then(|b| b.comma).filter(|&c| {
                    self.line_start(c) == self.line_start(e.start)
                        || (self.blank_text(c + 1, e.start) && !self.has_comment(c + 1..e.start))
                });
                let r = match (e.first_on_line, e.last_on_line) {
                    (true, Some(ce)) => self.line_start(e.start)..self.line_end(ce).1,
                    (false, Some(ce)) => match (e.comma, after_kept, sep) {
                        (None, false, Some(c)) => c..ce,
                        _ => prev_tok..ce,
                    },
                    (_, None) => match (e.comma, after_kept, sep) {
                        (Some(_), _, _) => e.start..after_comma.unwrap_or(e.end),
                        (None, _, Some(c)) => c..e.end,
                        (None, _, None) => e.start..e.end,
                    },
                };
                if first_removal.is_none() {
                    first_removal = Some(r.clone());
                }
                self.edit(r, "");
            }
            if appended.is_empty() {
                return;
            }
            let one_line = !self.raw[span.clone()].contains('\n');
            if let Some(&k) = kept.last() {
                let k_e = &entries[k];
                let removed_after = (k + 1..entries.len()).any(|i| removed[i]);
                if one_line {
                    let text: String = appended.iter().map(|a| format!(", {a}")).collect();
                    self.edit(k_e.end..k_e.end, text);
                } else if let Some(ce) = k_e.last_on_line {
                    let eol = self.eol(ce);
                    let indent = self.indent(k_e.start).to_string();
                    let had_comma = k_e.comma.is_some();
                    if !had_comma {
                        self.edit(k_e.end..k_e.end, ",");
                    }
                    let n = appended.len();
                    let text: String = appended
                        .iter()
                        .enumerate()
                        .map(|(t, a)| {
                            let c = if t + 1 < n || had_comma { "," } else { "" };
                            format!("{indent}{a}{c}{eol}")
                        })
                        .collect();
                    let pos = self.line_end(ce).1;
                    self.edit(pos..pos, text);
                } else {
                    let eol = self.eol(k_e.end);
                    let indent = self.indent(k_e.start).to_string();
                    match (k_e.comma, removed_after) {
                        (Some(c), false) => {
                            let text: String = appended
                                .iter()
                                .map(|a| format!("{eol}{indent}{a},"))
                                .collect();
                            self.edit(c + 1..c + 1, text);
                        }
                        _ => {
                            let text: String = appended
                                .iter()
                                .map(|a| format!(",{eol}{indent}{a}"))
                                .collect();
                            self.edit(k_e.end..k_e.end, text);
                        }
                    }
                }
            } else if let Some(e0) = entries.first() {
                let r = first_removal.unwrap_or(e0.start..e0.start);
                if e0.first_on_line && e0.last_on_line.is_some() {
                    let eol = self.eol(e0.start);
                    let indent = self.indent(e0.start).to_string();
                    let n = appended.len();
                    let text: String = appended
                        .iter()
                        .enumerate()
                        .map(|(t, a)| {
                            let c = if t + 1 < n || e0.comma.is_some() {
                                ","
                            } else {
                                ""
                            };
                            format!("{indent}{a}{c}{eol}")
                        })
                        .collect();
                    self.edit(r.start..r.start, text);
                } else {
                    let lead = self.raw[r.start..e0.start.max(r.start)].to_string();
                    self.edit(r.start..r.start, format!("{lead}{}", appended.join(", ")));
                }
            } else {
                let open = span.start + 1;
                let close = span.end - 1;
                if one_line {
                    let text = match inline {
                        true => format!(" {} ", appended.join(", ")),
                        false => appended.join(", "),
                    };
                    if self.blank(open, close) {
                        self.edit(open..close, text);
                    } else {
                        self.refuse("an empty inline table or array".into());
                    }
                } else {
                    let (ce, pos) = self.line_end(open);
                    let eol = self.eol(ce);
                    let indent = self.indent(close).to_string();
                    let n = appended.len();
                    let text: String = appended
                        .iter()
                        .enumerate()
                        .map(|(t, a)| {
                            let c = if t + 1 < n { "," } else { "" };
                            format!("{indent}{a}{c}{eol}")
                        })
                        .collect();
                    self.edit(pos..pos, text);
                }
            }
        }

        /// Only spacing and line ends between `a` and `b`.
        fn blank_text(&self, a: usize, b: usize) -> bool {
            a <= b
                && self.raw[a..b]
                    .bytes()
                    .all(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        }

        // -- keys --------------------------------------------------------------

        /// Write `key = v` (or a `[table]`, or `[[array]]` entries) where
        /// `cont` keeps its keys.
        fn insert(
            &mut self,
            cont: &mut Cont<'r>,
            prefix: &[String],
            key: &str,
            v: &toml::Value,
            at: &At,
        ) {
            let dotted: String = prefix
                .iter()
                .map(|k| format!("{}.", key_repr(k)))
                .chain([key_repr(key)])
                .collect();
            match cont {
                Cont::Seq(seq) => {
                    seq.appended.push(format!("{dotted} = {}", fresh_value(v)));
                }
                Cont::Body(t) => {
                    let t: &'r Table = t;
                    let is_root = at.header.len() == 1;
                    if prefix.is_empty() && (v.is_table() || is_aot(v)) {
                        self.insert_section(t, is_root, v, at);
                    } else {
                        self.insert_line(t, &format!("{dotted} = {}", fresh_value(v)));
                    }
                }
            }
        }

        /// A new `[table]` or `[[array]]` entries under `t`: at the end of the
        /// file for a top-level one, else after everything under `t`.
        fn insert_section(&mut self, t: &'r Table, is_root: bool, v: &toml::Value, at: &At) {
            let first_at_end = !self.appended_at_end;
            let (pos, eol, gap) = if is_root {
                let end = self.raw.len();
                let eol = self.eol(end.saturating_sub(1));
                // One blank line before each new section.
                let gap =
                    self.appended_at_end || (!self.raw.is_empty() && !self.blank_line_before_end());
                self.appended_at_end = true;
                (end, eol, gap)
            } else {
                let end = self.subtree_end(t);
                (self.line_end(end).1, self.eol(end), false)
            };
            let mut text = String::new();
            if pos == self.raw.len()
                && (first_at_end || !is_root)
                && !self.raw.is_empty()
                && !self.raw.ends_with('\n')
            {
                text.push_str(eol);
            }
            match v {
                toml::Value::Table(nt) => {
                    if gap {
                        text.push_str(eol);
                    }
                    text.push_str(&section(&at.header, false, nt, eol));
                }
                toml::Value::Array(a) => {
                    for nt in a.iter().filter_map(toml::Value::as_table) {
                        if gap {
                            text.push_str(eol);
                        }
                        text.push_str(&section(&at.header, true, nt, eol));
                    }
                }
                _ => {}
            }
            self.edit(pos..pos, text);
        }

        fn blank_line_before_end(&self) -> bool {
            self.raw.ends_with("\n\n") || self.raw.ends_with("\n\r\n")
        }

        /// A line after the last key line of `t`'s body, indented like it.
        fn insert_line(&mut self, t: &'r Table, line: &str) {
            let last = t
                .get_values()
                .into_iter()
                .filter_map(|(keys, v)| Some((keys.first()?.span()?.start, v.span()?.end)))
                .max_by_key(|(_, end)| *end);
            let (anchor, indent_at) = match (last, t.span()) {
                (Some((ks, ve)), _) => (ve, ks),
                (None, Some(h)) => (h.end, h.start),
                (None, None) if t.is_implicit() => {
                    self.refuse("a key in a table with no header".into());
                    return;
                }
                (None, None) => {
                    let p = self.bom();
                    let eol = self.eol(p);
                    self.edit(p..p, format!("{line}{eol}"));
                    return;
                }
            };
            let indent = self.indent(indent_at);
            let (ce, next) = self.line_end(anchor);
            let eol = self.eol(anchor);
            let text = if next == ce {
                format!("{eol}{indent}{line}")
            } else {
                format!("{indent}{line}{eol}")
            };
            self.edit(next..next, text);
        }

        /// Take `key` (under `prefix`) out of `cont`.
        fn remove(&mut self, cont: &mut Cont<'r>, prefix: &[String], key: &str, item: &'r Item) {
            match (cont, item) {
                (_, Item::ArrayOfTables(a)) => {
                    for t in a.iter() {
                        for r in self.subtree_ranges(t) {
                            self.delete_lines(r);
                        }
                    }
                }
                (_, Item::Table(t)) if !t.is_dotted() => {
                    for r in self.subtree_ranges(t) {
                        self.delete_lines(r);
                    }
                }
                (Cont::Seq(seq), _) => {
                    for (i, e) in seq.entries.iter().enumerate() {
                        if e.path.len() > prefix.len()
                            && e.path[..prefix.len()] == *prefix
                            && e.path[prefix.len()] == key
                        {
                            seq.removed[i] = true;
                        }
                    }
                }
                (Cont::Body(t), _) => {
                    let lines: Vec<(usize, usize)> = t
                        .get_values()
                        .into_iter()
                        .filter(|(keys, _)| {
                            keys.len() > prefix.len()
                                && keys[..prefix.len()]
                                    .iter()
                                    .map(|k| k.get())
                                    .eq(prefix.iter().map(String::as_str))
                                && keys[prefix.len()].get() == key
                        })
                        .filter_map(|(keys, v)| Some((keys.last()?.span()?.start, v.span()?.end)))
                        .collect();
                    for (ks, ve) in lines {
                        let ls = self.line_start(ks);
                        let (_, next) = self.line_end(ve);
                        self.delete_lines(ls..next);
                    }
                }
            }
        }

        /// Delete whole lines `r`; the last line of a file without a final
        /// line end takes the line end before it, so the file still has none.
        fn delete_lines(&mut self, r: Range<usize>) {
            if r.end == self.raw.len() && !self.raw.ends_with('\n') && r.start > 0 {
                let before = if self.raw[..r.start].ends_with("\r\n") {
                    2
                } else {
                    1
                };
                self.edit(r.start - before..r.end, "");
            } else {
                self.edit(r, "");
            }
        }

        // -- applying -----------------------------------------------------------

        /// The edited text.
        pub(super) fn apply(&self) -> Result<String, String> {
            apply_within(self.raw, &(0..self.raw.len()), &self.edits)
        }
    }

    fn extend(prefix: &[String], key: &str) -> Vec<String> {
        prefix.iter().cloned().chain([key.to_string()]).collect()
    }

    fn is_aot(v: &toml::Value) -> bool {
        matches!(v, toml::Value::Array(a) if !a.is_empty() && a.iter().all(toml::Value::is_table))
    }

    /// `raw[within]` with `edits` (all inside it) applied: overlapping and
    /// touching deletions merged, one blank line kept where a deletion of
    /// whole lines would make two meet.
    fn apply_within(raw: &str, within: &Range<usize>, edits: &[Edit]) -> Result<String, String> {
        let mut edits: Vec<Edit> = edits.to_vec();
        edits.sort_by_key(|e| (e.at.start, e.at.end));
        // Merge deletions that overlap, touch, or are apart by blank lines.
        let mut merged: Vec<Edit> = Vec::new();
        for e in edits {
            if let Some(last) = merged.last_mut()
                && last.text.is_empty()
                && e.text.is_empty()
                && !last.at.is_empty()
                && !e.at.is_empty()
                && (e.at.start <= last.at.end
                    || (blank_lines_between(raw, last.at.end, e.at.start)
                        && raw.as_bytes()[e.at.start - 1] == b'\n'))
            {
                last.at.end = last.at.end.max(e.at.end);
                continue;
            }
            merged.push(e);
        }
        // One blank line where two would meet.
        let n = merged.len();
        for i in 0..n {
            let e = &merged[i];
            if !e.text.is_empty() || e.at.is_empty() {
                continue;
            }
            let (a, b) = (e.at.start, e.at.end);
            let whole = (a == 0 || raw.as_bytes()[a - 1] == b'\n')
                && (b == raw.len() || raw.as_bytes()[b - 1] == b'\n');
            let next_starts_here = merged.get(i + 1).is_some_and(|f| f.at.start == b);
            if whole && a > 0 && b < raw.len() && !next_starts_here {
                let prev_ls = raw[..a - 1].rfind('\n').map(|x| x + 1).unwrap_or(0);
                let next_end = raw[b..].find('\n').map(|x| b + x + 1).unwrap_or(raw.len());
                let is_blank = |s: &str| s.trim().is_empty();
                if is_blank(&raw[prev_ls..a]) && is_blank(&raw[b..next_end]) {
                    merged[i].at.end = next_end;
                }
            }
        }
        let mut out = String::with_capacity(within.len());
        let mut cursor = within.start;
        for e in &merged {
            if e.at.start < cursor || e.at.end > within.end {
                return Err("two edits of the file overlap".into());
            }
            out.push_str(&raw[cursor..e.at.start]);
            out.push_str(&e.text);
            cursor = e.at.end;
        }
        out.push_str(&raw[cursor..within.end]);
        Ok(out)
    }

    fn blank_lines_between(raw: &str, a: usize, b: usize) -> bool {
        a <= b
            && (a == 0 || raw.as_bytes()[a - 1] == b'\n')
            && raw[a..b].trim().is_empty()
            && raw[a..b].contains('\n')
    }

    /// A new `[header]` (or `[[header]]`) section holding `t`: its values,
    /// then its tables as sections of their own.
    fn section(header: &[String], aot: bool, t: &toml::Table, eol: &str) -> String {
        let h = header.join(".");
        let mut out = match aot {
            true => format!("[[{h}]]{eol}"),
            false => format!("[{h}]{eol}"),
        };
        for (k, v) in t {
            if !v.is_table() && !is_aot(v) {
                out.push_str(&format!("{} = {}{eol}", key_repr(k), fresh_value(v)));
            }
        }
        for (k, v) in t {
            let sub: Vec<String> = header.iter().cloned().chain([key_repr(k)]).collect();
            match v {
                toml::Value::Table(st) => out.push_str(&section(&sub, false, st, eol)),
                toml::Value::Array(a) if is_aot(v) => {
                    for st in a.iter().filter_map(toml::Value::as_table) {
                        out.push_str(&section(&sub, true, st, eol));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// What of `nv` (a value the file leaves out, unchanged) a save writes
    /// out: the [`PINNED`] keys at or under `schema`.
    fn pinned_part(schema: &[&str], nv: &toml::Value) -> Option<toml::Value> {
        if PINNED.contains(&schema) {
            return Some(nv.clone());
        }
        let below = PINNED
            .iter()
            .any(|p| p.len() > schema.len() && p[..schema.len()] == *schema);
        if !below {
            return None;
        }
        match nv {
            toml::Value::Table(t) => {
                let part: toml::Table = t
                    .iter()
                    .filter_map(|(k, v)| {
                        let s: Vec<&str> = schema.iter().copied().chain([k.as_str()]).collect();
                        Some((k.clone(), pinned_part(&s, v)?))
                    })
                    .collect();
                (!part.is_empty()).then_some(toml::Value::Table(part))
            }
            _ => None,
        }
    }

    /// What of table `nt` a save writes into a file that left the table out
    /// while `old_t` held its defaults: the keys that changed, and the pinned.
    fn changed_part(schema: &[&str], old_t: &toml::Table, nt: &toml::Table) -> toml::Table {
        nt.iter()
            .filter_map(|(k, v)| {
                let s: Vec<&str> = schema.iter().copied().chain([k.as_str()]).collect();
                match old_t.get(k) {
                    Some(o) if o == v => Some((k.clone(), pinned_part(&s, v)?)),
                    Some(toml::Value::Table(ost)) => match v {
                        toml::Value::Table(nst) => {
                            Some((k.clone(), toml::Value::Table(changed_part(&s, ost, nst))))
                        }
                        _ => Some((k.clone(), v.clone())),
                    },
                    _ => Some((k.clone(), v.clone())),
                }
            })
            .collect()
    }

    /// `(legacy key, the key it feeds)`: read on load, never written. A
    /// target's `serial` feeds `serials` (`TargetRaw`).
    const LEGACY_KEYS: &[(&str, &str)] = &[("serial", "serials")];

    /// What matches an array element across a save: its `label` (sources,
    /// targets) or `name` (subvolume entries). Nothing else is matched.
    fn identity(v: &toml::Value) -> Option<&str> {
        let t = v.as_table()?;
        t.get("label").or_else(|| t.get("name"))?.as_str()
    }

    /// For each element of `na`, the index of the `oa` element it continues:
    /// first an element whose known content is unchanged (so a plain value
    /// keeps its comment, and of two same-named entries the one removed is the
    /// one whose content went), then the unused one of the same identity that
    /// shares the most known values with it, the earlier on a tie.
    /// Same-named entries whose known content is identical cannot be told
    /// apart; the earlier ones are taken first. A rename is never inferred:
    /// an element whose `label` / `name` changed is written fresh and the old
    /// one dropped (bd DAS-Backup-Manager-m0pr.2), so a writer that renames
    /// must carry over whatever the element should keep itself.
    fn pick(oa: &[toml::Value], na: &[toml::Value]) -> Vec<Option<usize>> {
        let mut used = vec![false; oa.len()];
        let mut picks = vec![None; na.len()];
        for (j, ne) in na.iter().enumerate() {
            if let Some(i) = (0..oa.len()).find(|&i| !used[i] && oa[i] == *ne) {
                used[i] = true;
                picks[j] = Some(i);
            }
        }
        for (j, ne) in na.iter().enumerate() {
            let Some(id) = identity(ne).filter(|_| picks[j].is_none()) else {
                continue;
            };
            let best = (0..oa.len())
                .filter(|&i| !used[i] && identity(&oa[i]) == Some(id))
                // max_by_key keeps the LAST maximum: reversed, the earliest.
                .rev()
                .max_by_key(|&i| shared_values(&oa[i], ne));
            if let Some(i) = best {
                used[i] = true;
                picks[j] = Some(i);
            }
        }
        picks
    }

    /// How many of `b`'s keys `a` holds with the same value (tables only).
    fn shared_values(a: &toml::Value, b: &toml::Value) -> usize {
        match (a.as_table(), b.as_table()) {
            (Some(a), Some(b)) => b.iter().filter(|(k, v)| a.get(*k) == Some(*v)).count(),
            _ => 0,
        }
    }

    /// The warning for old element `i` of the array at `path`, dropped from
    /// the file (`t`, what this binary knew of it: `old_t`), when it took
    /// keys this binary does not read, or a comment of its own, with it. None
    /// when it held only known keys: those are this config's to remove.
    fn dropped_warning(
        path: &str,
        i: usize,
        t: &dyn TableLike,
        old_t: &toml::Table,
        comment: bool,
    ) -> Option<String> {
        // What the loader reads, not what the serialiser writes: a key it
        // leaves out at its default (`after_index = false`) is still known.
        let schema = element_schema(path).unwrap_or_else(|| old_t.clone());
        let mut unknown = Vec::new();
        unknown_in(t, &schema, "", &mut unknown);
        let what = match (unknown.is_empty(), comment) {
            (true, false) => return None,
            (true, true) => "its comments".to_string(),
            (false, false) => format!("keys this version does not read ({})", unknown.join(", ")),
            (false, true) => format!(
                "keys this version does not read ({}) and its comments",
                unknown.join(", ")
            ),
        };
        let named = old_t
            .get("label")
            .or_else(|| old_t.get("name"))
            .and_then(toml::Value::as_str)
            .map(|id| format!(" \"{id}\""))
            .unwrap_or_default();
        Some(format!(
            "{path}[{i}]{named} is no longer in the config and was removed with {what}; \
             a renamed element is written fresh, so anything it should keep must be \
             added to it again"
        ))
    }

    /// What this version reads of an element of the array at `path`
    /// (`source[0].subvolumes`): that element in [`super::key_schema`].
    fn element_schema(path: &str) -> Option<toml::Table> {
        let mut at = toml::Value::Table(super::key_schema());
        for step in path.split('.') {
            let name = step.split('[').next().unwrap_or(step);
            at = at.as_table()?.get(name)?.clone();
            if let toml::Value::Array(a) = &at {
                at = a.first()?.clone();
            }
        }
        match at {
            toml::Value::Table(t) => Some(t),
            _ => None,
        }
    }

    /// The keys of `doc` — and of the tables and array elements inside it —
    /// that `schema` (what this version reads there) lacks, dotted from `doc`.
    fn unknown_in(doc: &dyn TableLike, schema: &toml::Table, at: &str, out: &mut Vec<String>) {
        for (k, item) in doc.iter() {
            let here = match at.is_empty() {
                true => k.to_string(),
                false => format!("{at}.{k}"),
            };
            match schema.get(k) {
                None => out.push(here),
                Some(toml::Value::Table(st)) => {
                    if let Some(sub) = item.as_table_like() {
                        unknown_in(sub, st, &here, out);
                    }
                }
                Some(toml::Value::Array(sa)) => {
                    if let Some(toml::Value::Table(est)) = sa.first() {
                        for (i, el) in table_elements(item) {
                            unknown_in(el, est, &format!("{here}[{i}]"), out);
                        }
                    }
                }
                Some(_) => {}
            }
        }
    }

    /// The elements of an array of tables, or the inline tables of an inline
    /// array (a bare string spells out no keys), each with its index in the
    /// array as the file writes it: a bare string counts.
    fn table_elements(item: &Item) -> Vec<(usize, &dyn TableLike)> {
        match item {
            Item::ArrayOfTables(a) => a
                .iter()
                .enumerate()
                .map(|(i, t)| (i, t as &dyn TableLike))
                .collect(),
            Item::Value(Value::Array(a)) => a
                .iter()
                .enumerate()
                .filter_map(|(i, v)| Some((i, v.as_inline_table()? as &dyn TableLike)))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A value as toml_edit writes a new one.
    fn fresh_value(v: &toml::Value) -> Value {
        match v {
            toml::Value::String(s) => Value::from(s.as_str()),
            toml::Value::Integer(i) => Value::from(*i),
            toml::Value::Float(f) => Value::from(*f),
            toml::Value::Boolean(b) => Value::from(*b),
            toml::Value::Datetime(d) => Value::from(*d),
            toml::Value::Array(a) => Value::Array(a.iter().map(fresh_value).collect()),
            toml::Value::Table(t) => {
                Value::InlineTable(t.iter().map(|(k, v)| (k.clone(), fresh_value(v))).collect())
            }
        }
    }

    /// The keys that decide WHAT is backed up, WHERE it goes and for HOW LONG
    /// it is kept, and have a compiled default. A save writes each one out,
    /// with its current value, when the file leaves it out: a later version
    /// changing that default must never change an existing host's backups
    /// silently. `*` is every element of an array of tables.
    ///
    /// - `general.btrbk_conf`: the file btrbk.conf is rendered to, which btrbk
    ///   runs from
    /// - `boot.*`: whether boot subvolumes are archived and replaced, which,
    ///   and how long the archives are kept
    /// - `subvolumes.exclude`, `doctor.exclude`: what the run never adopts
    /// - `restore.allowed_roots`: where a restore may write (a security
    ///   boundary the root helper enforces)
    /// - `source[*].snapshot_dir`, `target_subdirs`, `target_labels`: where a
    ///   source's snapshots are taken and sent, and to which targets
    /// - `target[*].retention.*`: how long a target keeps them (an absent
    ///   period is 0, which changes btrbk's `target_preserve`)
    ///
    /// Not here: defaults whose absence is itself the value (`Option`s, and
    /// `after_index` = false), advisory ones (`serials`,
    /// `display_name`), and ones that decide neither scope nor retention
    /// (logs, `[das]`, `[scrub]`, `[recovery_os]`, email, GUI).
    ///
    /// Each subvolume entry's snapshot name is pinned too, by the config
    /// itself rather than this list: [`super::Config::with_pinned_snapshot_names`].
    pub(super) const PINNED: &[&[&str]] = &[
        &["general", "btrbk_conf"],
        &["boot", "enabled"],
        &["boot", "subvolumes"],
        &["boot", "archive_retention_days"],
        &["subvolumes", "exclude"],
        &["doctor", "exclude"],
        &["restore", "allowed_roots"],
        &["source", "*", "snapshot_dir"],
        &["source", "*", "target_subdirs"],
        &["source", "*", "target_labels"],
        &["target", "*", "retention", "daily"],
        &["target", "*", "retention", "weekly"],
        &["target", "*", "retention", "monthly"],
        &["target", "*", "retention", "yearly"],
    ];

    /// Every path at which `a` and `b` differ, dotted (`target[1].serials`):
    /// `a`'s keys in order, then keys only `b` has. Elements of arrays of
    /// tables are compared one by one; any other value whole.
    pub(super) fn differing_paths(a: &toml::Table, b: &toml::Table) -> Vec<String> {
        let mut out = Vec::new();
        diff_tables("", a, b, &mut out);
        out
    }

    fn diff_tables(prefix: &str, a: &toml::Table, b: &toml::Table, out: &mut Vec<String>) {
        let at = |k: &str| {
            if prefix.is_empty() {
                k.to_string()
            } else {
                format!("{prefix}.{k}")
            }
        };
        for (k, av) in a {
            match b.get(k) {
                Some(bv) => diff_values(&at(k), av, bv, out),
                None => out.push(at(k)),
            }
        }
        for k in b.keys() {
            if !a.contains_key(k) {
                out.push(at(k));
            }
        }
    }

    fn diff_values(path: &str, a: &toml::Value, b: &toml::Value, out: &mut Vec<String>) {
        match (a, b) {
            (toml::Value::Table(x), toml::Value::Table(y)) => diff_tables(path, x, y, out),
            (toml::Value::Array(x), toml::Value::Array(y))
                if x.iter().chain(y).all(toml::Value::is_table) =>
            {
                for i in 0..x.len().max(y.len()) {
                    match (x.get(i), y.get(i)) {
                        (Some(u), Some(v)) => diff_values(&format!("{path}[{i}]"), u, v, out),
                        _ => out.push(format!("{path}[{i}]")),
                    }
                }
            }
            _ => {
                if a != b {
                    out.push(path.to_string());
                }
            }
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
    /// needed. Every writer of `config.toml` comes through here or through
    /// [`Config::save_text`] (the GUI's editor); both write by
    /// `Config::edited_into_with`, so neither can drop what the other keeps. The
    /// one file written whole elsewhere is a recovery drive's own
    /// `config.toml`, which `das-recovery-state-sync.sh` regenerates from the
    /// host's at every boot (`config derive-recovery` into a new file).
    ///
    /// An existing file is EDITED, not regenerated (bd DAS-Backup-Manager-28il):
    /// only the values this config changes are rewritten, so keys this binary
    /// does not know (written by a newer version, or by the operator before
    /// installing the version that reads them) and every comment survive, and
    /// an older binary's save cannot erase a newer one's settings. See
    /// `Config::edited_into_with` for the rules. A missing file is written fresh.
    /// An existing file that is not a config this version can read is refused,
    /// unchanged: with nothing known about it, regenerating it would erase
    /// whatever it holds.
    ///
    /// The file is replaced in one step and keeps its mode (and, saved by
    /// root, its owner and group) — see [`crate::fsutil::write_atomic`]. Every
    /// error names `path`, once.
    pub fn save(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        self.save_with(path, &|_| {})
    }

    /// [`Config::save`] with a hook run on the edited text before it is
    /// checked: a test seam, so a test can inject the edit fault the
    /// read-back check exists to stop. Each warning of the save goes to
    /// stderr as `Warning: <path>: …`.
    #[doc(hidden)]
    pub fn save_with(
        &self,
        path: &Path,
        fault: &dyn Fn(&mut String),
    ) -> Result<(), Box<dyn Error>> {
        self.save_reporting(path, fault, &mut |w| {
            eprintln!("Warning: {}: {w}", path.display())
        })
    }

    /// [`Config::save_with`] handing each warning of the save to `warn` once
    /// the file is written, instead of printing it on stderr (where the CLI's
    /// terminal and the helper's journal see it): an array element removed
    /// with keys this version does not read, or with comments of its own
    /// (`Config::edited_into_with`). A warning never refuses the save.
    #[doc(hidden)]
    pub fn save_reporting(
        &self,
        path: &Path,
        fault: &dyn Fn(&mut String),
        warn: &mut dyn FnMut(String),
    ) -> Result<(), Box<dyn Error>> {
        let named = |e: &dyn std::fmt::Display| format!("cannot write {}: {e}", path.display());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| named(&e))?;
        }
        let (text, warnings) = match fs::read_to_string(path) {
            Ok(existing) => self
                .edited_into_with(&existing, fault)
                .map_err(|e| named(&e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let body = self
                    .with_pinned_snapshot_names()
                    .to_toml()
                    .map_err(|e| named(&e))?;
                (format!("{FRESH_HEADER}{body}"), Vec::new())
            }
            Err(e) => return Err(named(&e).into()),
        };
        let written = crate::fsutil::write_atomic(path, &text);
        // Reported once the new file is in place — even when a step after
        // the rename (the directory flush) failed: the removal reached the
        // disk. Read back, never inferred from the error.
        if written.is_ok() || fs::read_to_string(path).is_ok_and(|on_disk| on_disk == text) {
            warnings.into_iter().for_each(warn);
        }
        written?;
        Ok(())
    }

    /// Save `text` — a whole `config.toml` as the operator typed it (the GUI's
    /// editor) — to `path`, as typed: comments and keys this version does not
    /// know are kept verbatim. The only change made to it is the one every
    /// save makes, writing out the backup-scoping keys and snapshot names it
    /// leaves out (`Config::edited_into_with`). Refused, with nothing written, when `text`
    /// is not a config this version can read. Atomic, mode kept, every error
    /// naming `path` once, as [`Config::save`].
    ///
    /// Whatever the file held is replaced by `text`: a change another writer
    /// made after the editor loaded the file is lost, as with any whole-file
    /// edit (last writer wins, bd DAS-Backup-Manager-lxw).
    pub fn save_text(text: &str, path: &Path) -> Result<(), Box<dyn Error>> {
        let named = |e: &dyn std::fmt::Display| format!("cannot write {}: {e}", path.display());
        let cfg = Self::from_toml(text).map_err(|e| {
            named(&format!(
                "the submitted text is not a config this version can read ({e}); \
                 the file was left unchanged"
            ))
        })?;
        // `text` is edited into itself: no element is ever dropped, so there
        // is nothing to warn about.
        let (out, _) = cfg.edited_into_with(text, &|_| {}).map_err(|e| named(&e))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| named(&e))?;
        }
        crate::fsutil::write_atomic(path, &out)?;
        Ok(())
    }

    /// `existing` (the text of `config.toml`) with this config written into
    /// it, and the save's warnings. What this binary KNOWS of the file is
    /// what it would write after loading it (`from_toml` then `to_toml`).
    /// The file is parsed with its spans (`toml_edit::Document`) and every
    /// difference between what is known and this config becomes a splice on
    /// the original bytes (`mod splice`); every byte outside a splice is
    /// copied as it is — strings, comments, blank lines, indentation, line
    /// endings (inside multi-line strings too), a byte-order mark, a missing
    /// final newline, the order of dotted keys. Per key:
    ///
    /// - known and unchanged: untouched, in the operator's spelling;
    /// - changed: its value's bytes replaced, its comment and spacing kept;
    /// - new: written on a line of its own after the last key line of its
    ///   table, indented like that line, with that line's line ending (in an
    ///   inline table or array, after the last entry: on its line in a
    ///   one-line one, on a line of its own in a multi-line one); a new table
    ///   at the top level goes at the end of the file. An omitted key whose
    ///   default is unchanged stays omitted;
    /// - known but no longer in this config (an entry deleted, an `Option`
    ///   cleared): its own text taken out — a key alone on its line, the
    ///   line; an inline entry, itself, its comma and the comment ending its
    ///   line when it ends it (the comma before it, when it was the last one);
    /// - unknown: kept.
    ///
    /// Arrays of tables (`[[source]]`, `[[target]]`, subvolume entries) are
    /// matched element by element on their `label` (or `name`): a matched
    /// element is edited by the same rules and keeps its own unknown keys
    /// and comments, wherever it moves; an element with no match is written
    /// fresh after the last one in the file, and an old one with no match is
    /// removed with everything in it — its header's line to its last line,
    /// its own tables and entries included. The comment and blank lines
    /// around a removed element (above its header, under its last line)
    /// stay; one blank line is kept where two would meet. An element renamed
    /// is a removal plus an addition: renames are never inferred (bd
    /// DAS-Backup-Manager-m0pr.2), and a removed element that held keys this
    /// version does not read, or comments of its own, is named in a warning,
    /// by its index in the file. Elements are matched on unchanged known
    /// content first, so a plain array's comments stay with their values and
    /// of two same-named entries the right one goes.
    ///
    /// A legacy key this version reads but never writes (a target's `serial`)
    /// is removed when the key it feeds changes, and a removed key
    /// (`REMOVED_KEYS`) from every subvolume entry. Every backup-scoping key
    /// the file leaves out is written out with its current value
    /// (`splice::PINNED`), and so is every entry's snapshot name
    /// ([`Config::with_pinned_snapshot_names`]).
    ///
    /// Refused, with nothing written, when `existing` is not TOML, is not a
    /// config this version can read, an edit has no place in the file, or the
    /// edited text would not read back as exactly this config; the refusal
    /// names the paths that would differ.
    fn edited_into_with(
        &self,
        existing: &str,
        fault: &dyn Fn(&mut String),
    ) -> Result<(String, Vec<String>), String> {
        let unchanged = "the file was left unchanged";
        let doc = toml_edit::Document::parse(existing)
            .map_err(|e| format!("the file on disk is not TOML ({e}); {unchanged}"))?;
        let known = Self::from_toml(existing).map_err(|e| {
            format!("the file on disk is not a config this version can read ({e}); {unchanged}")
        })?;
        let want = self
            .with_pinned_snapshot_names()
            .to_toml()
            .map_err(|e| e.to_string())?;
        let as_table = |text: &str| toml::from_str::<toml::Table>(text).map_err(|e| e.to_string());
        let old = as_table(&known.to_toml().map_err(|e| e.to_string())?)?;
        let new = as_table(&want)?;
        let plan = splice::plan(existing, &doc, &old, &new);
        if !plan.unplaceable.is_empty() {
            return Err(format!(
                "the edit has no place in the file ({}); {unchanged}",
                plan.unplaceable.join(", ")
            ));
        }
        let mut out = plan.apply().map_err(|e| format!("{e}; {unchanged}"))?;
        fault(&mut out);
        let back = Self::from_toml(&out)
            .map_err(|e| e.to_string())
            .and_then(|c| c.to_toml().map_err(|e| e.to_string()))
            .and_then(|t| as_table(&t))
            .map_err(|e| format!("the edited file would not load ({e}); {unchanged}"))?;
        if back != new {
            return Err(format!(
                "the edited file would not read back as the config being saved \
                 ({} would differ); {unchanged}",
                splice::differing_paths(&new, &back).join(", ")
            ));
        }
        Ok((out, plan.warnings))
    }

    /// This config with every subvolume entry's snapshot name written out:
    /// where `snapshot_name` is unset, the name btrbk is given for it today,
    /// from the renderer's own derivation
    /// ([`crate::btrbk_conf::resolve_snapshot_names`]). Saved, a later change
    /// to that derivation can never rename a series and restart its send
    /// chain. A top-level entry (`.`) is left as it is: it must name itself.
    pub fn with_pinned_snapshot_names(&self) -> Config {
        let mut out = self.clone();
        for source in &mut out.sources {
            let names = crate::btrbk_conf::resolve_snapshot_names(&source.subvolumes);
            for (entry, name) in source.subvolumes.iter_mut().zip(names) {
                if entry.snapshot_name.is_none() && entry.name != crate::adopt::TOP_LEVEL {
                    entry.snapshot_name = Some(name);
                }
            }
        }
        out
    }

    /// What the GUI's Backup panel shows, as JSON: the source and target
    /// labels it offers and whether the boot step runs. Read from the parsed
    /// config, never from the file's lines (bd DAS-Backup-Manager-28il).
    pub fn panel_json(&self) -> String {
        serde_json::json!({
            "sources": self.sources.iter().map(|s| &s.label).collect::<Vec<_>>(),
            "targets": self.targets.iter().map(|t| &t.label).collect::<Vec<_>>(),
            "boot_enabled": self.boot.enabled,
        })
        .to_string()
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

    /// Every list that holds exactly `pattern`, in the order
    /// [`exclude_patterns`](Self::exclude_patterns) reads them: `built-in`,
    /// `[subvolumes].exclude`, `[doctor].exclude`. Empty when none does. The
    /// coverage report names them all, so removing the pattern from one list
    /// is never mistaken for removing it.
    pub fn exclude_pattern_origins(&self, pattern: &str) -> Vec<&'static str> {
        [
            ("built-in", DEFAULT_EXCLUDES.contains(&pattern)),
            (
                "[subvolumes].exclude",
                self.subvolumes.exclude.iter().any(|p| p == pattern),
            ),
            (
                "[doctor].exclude",
                self.doctor.exclude.iter().any(|p| p == pattern),
            ),
        ]
        .into_iter()
        .filter(|(_, holds)| *holds)
        .map(|(origin, _)| origin)
        .collect()
    }

    /// The uniqueness and shape rules the boot step's input must meet, in ONE
    /// place: `validate` includes them, and `backup::boot_plan` (so `backup
    /// boot-plan`, the script's boot step and `archive_boot`) enforces them
    /// itself, because `Config::load` does not run `validate`.
    pub fn boot_input_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        // The boot step keys a target's state by its label, and a label names a
        // mount in every log line: two targets sharing one would overwrite each
        // other's entry (bd azvo).
        let mut labels = std::collections::HashSet::new();
        let mut mounts = std::collections::HashSet::new();
        for tgt in &self.targets {
            if !labels.insert(tgt.label.as_str()) {
                errors.push(format!(
                    "Target label '{}' is used by more than one [[target]] — labels must be unique",
                    tgt.label
                ));
            }
            // Compared as one directory however it is spelled ('/mnt/t',
            // '/mnt/t/'): bd DAS-Backup-Manager-2jkw.
            if !mounts.insert(crate::btrbk_conf::normalise_dir(&tgt.mount)) {
                errors.push(format!(
                    "Target mount '{}' is used by more than one [[target]] — mounts must be unique",
                    tgt.mount
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
            // `.` would snapshot the target's whole top level; `..` leaves it.
            if sv.split('/').any(|c| c == "." || c == "..") {
                errors.push(format!(
                    "[boot].subvolumes entry '{sv}' has a '.' or '..' path component"
                ));
            }
        }
        // Each source's series lives in its own target directory, and the boot
        // step rebuilds from the newest snapshot it finds: a boot subvolume
        // two sources list could be rebuilt from the other volume's OS. A
        // retired entry counts, its series is still on the targets
        // (bd DAS-Backup-Manager-5237.12).
        let mut checked = std::collections::HashSet::new();
        for sv in self.boot.subvolumes.iter().filter(|sv| checked.insert(*sv)) {
            let listing: Vec<&str> = self
                .sources
                .iter()
                .filter(|s| s.subvolumes.iter().any(|e| e.name == *sv))
                .map(|s| s.label.as_str())
                .collect();
            if listing.len() > 1 {
                errors.push(format!(
                    "[boot].subvolumes entry '{sv}' is listed by more than one source ({}) — \
                     the boot step cannot tell whose snapshots rebuild it; one source may list it \
                     (or take it out of [boot].subvolumes)",
                    listing.join(", ")
                ));
            }
        }
        errors
    }

    /// The rules on `[[source.subvolumes]]` names and on exclude patterns
    /// that the volume's top level (`.`, btrbk's `subvolume .`) calls for
    /// (bd DAS-Backup-Manager-xtnq):
    ///
    /// - `.` alone names the top level. It needs an explicit `snapshot_name`
    ///   (otherwise its snapshots would be `.<timestamp>`), may appear once
    ///   per volume, and — unless retired — may sit only in a source that
    ///   sends to no `mirror` target: it has no configured ancestor, so it
    ///   goes to the primary only.
    /// - Every other name is a relative path with no empty, `.` or `..`
    ///   component, so no consumer computes a path like `<vol>/./x`.
    /// - An exclude pattern may not be empty or `.`: neither matches any
    ///   subvolume, so either would be accepted and ignored. The top level
    ///   is excluded by `/`.
    pub fn subvolume_name_errors(&self) -> Vec<String> {
        use crate::adopt::{TOP_LEVEL, TOP_LEVEL_EXCLUDE};
        let mut errors = Vec::new();
        let mut top_levels: Vec<&str> = Vec::new();
        for src in &self.sources {
            for sv in &src.subvolumes {
                if sv.name != TOP_LEVEL {
                    if sv
                        .name
                        .split('/')
                        .any(|c| c.is_empty() || c == "." || c == "..")
                    {
                        errors.push(format!(
                            "Subvolume '{}' in source '{}' must be a relative path with no empty, \
                             '.' or '..' component ('{TOP_LEVEL}' alone names the volume's top level)",
                            sv.name, src.label
                        ));
                    }
                    continue;
                }
                if top_levels.contains(&src.volume.as_str()) {
                    errors.push(format!(
                        "The top level ('{TOP_LEVEL}') of volume '{}' is configured more than once \
                         (again in source '{}')",
                        src.volume, src.label
                    ));
                } else {
                    top_levels.push(&src.volume);
                }
                if sv.snapshot_name.is_none() {
                    errors.push(format!(
                        "The top level ('{TOP_LEVEL}') of volume '{}' in source '{}' needs an \
                         explicit snapshot_name",
                        src.volume, src.label
                    ));
                }
            }
        }
        errors.extend(self.top_level_scope_errors());
        for src in &self.sources {
            // `snapshot_dir` is a directory inside the volume: empty, absolute,
            // `.`, or with an empty or `..` component, it yields
            // `<volume>//<name>`, `<volume>/./<name>` (the snapshot step's
            // filter) or a path outside the volume. One trailing `/` is fine.
            let dir = src
                .snapshot_dir
                .strip_suffix('/')
                .unwrap_or(&src.snapshot_dir);
            if src.snapshot_dir.starts_with('/')
                || dir
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")
            {
                errors.push(format!(
                    "Source '{}' has snapshot_dir '{}' — it must be a path inside its volume, \
                     relative, with no empty, '.' or '..' component",
                    src.label, src.snapshot_dir
                ));
            }
        }
        for pattern in self.exclude_patterns() {
            if pattern.is_empty() || pattern == TOP_LEVEL {
                errors.push(format!(
                    "Exclude pattern '{pattern}' matches no subvolume — write '{TOP_LEVEL_EXCLUDE}' \
                     to exclude each volume's top level"
                ));
            }
        }
        errors
    }

    /// The labels of the `mirror` targets `source`'s volume block sends to —
    /// every mirror when `target_labels` is empty, else the ones it names.
    pub fn mirrors_reached(&self, source: &Source) -> Vec<&str> {
        self.targets
            .iter()
            .filter(|t| t.role == TargetRole::Mirror)
            .filter(|t| source.target_labels.is_empty() || source.target_labels.contains(&t.label))
            .map(|t| t.label.as_str())
            .collect()
    }

    /// A live (not retired) top-level entry (`.`) in a source that sends to a
    /// mirror target: the top level goes to the primary target only. Checked
    /// by `validate` AND by the `btrbk.conf` renderer itself, so no caller
    /// that skips validation (`setup --upgrade`, the wizard) can write a file
    /// that sends a volume's top level to a recovery drive (bd xtnq D1).
    pub fn top_level_scope_errors(&self) -> Vec<String> {
        use crate::adopt::TOP_LEVEL;
        let mut errors = Vec::new();
        for src in &self.sources {
            let live_top = src
                .subvolumes
                .iter()
                .any(|sv| sv.name == TOP_LEVEL && sv.retired.is_none());
            let mirrors = self.mirrors_reached(src);
            if live_top && !mirrors.is_empty() {
                errors.push(format!(
                    "The top level ('{TOP_LEVEL}') of volume '{}' is in source '{}', which sends \
                     to mirror target(s) '{}' — the top level goes to the primary target only",
                    src.volume,
                    src.label,
                    mirrors.join("', '")
                ));
            }
        }
        errors
    }

    /// The mount of the target a snapshot should be read from first: on a
    /// recovery drive's own OS (`general.recovery_drive`) the booted drive,
    /// which `derive-recovery` puts at `targets[0]`; `None` on a host.
    pub fn recovery_preferred_root(&self) -> Option<String> {
        if self.general.recovery_drive {
            self.targets.first().map(|t| t.mount.clone())
        } else {
            None
        }
    }

    /// The configuration a recovery drive's own OS runs with, derived from the
    /// host's (spec D4): no sources; every target kept, the one whose
    /// filesystem the OS booted from (by `mount_uuid`) moved first and the rest
    /// left in their order, so an attached 22 TB pair is still mounted by UUID
    /// for what only it holds (`recovery_preferred_root` takes `targets[0]`);
    /// email, scrub and the boot step off; restores allowed only under
    /// `RECOVERY_ALLOWED_ROOTS`. Refuses when no target, or more than one,
    /// names `booted_uuid` — never a guess. Target `mount` strings are kept
    /// as they are: the index's `target_root` was recorded with the same ones.
    pub fn derive_recovery(&self, booted_uuid: &str) -> Result<Config, String> {
        let matching: Vec<usize> = self
            .targets
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.mount_uuid
                    .as_deref()
                    .is_some_and(|u| u.eq_ignore_ascii_case(booted_uuid))
            })
            .map(|(i, _)| i)
            .collect();
        let booted = match matching.as_slice() {
            [one] => *one,
            [] => {
                let known: Vec<&str> = self.targets.iter().map(|t| t.label.as_str()).collect();
                return Err(format!(
                    "no target of filesystem {booted_uuid} in this configuration (targets: {})",
                    known.join(", ")
                ));
            }
            many => {
                let labels: Vec<&str> = many
                    .iter()
                    .map(|&i| self.targets[i].label.as_str())
                    .collect();
                return Err(format!(
                    "two targets name filesystem {booted_uuid}: {} — fix the configuration, nothing is derived",
                    labels.join(", ")
                ));
            }
        };
        let mut r = self.clone();
        r.general.recovery_drive = true;
        r.sources.clear();
        let own = r.targets.remove(booted);
        r.targets.insert(0, own);
        r.email.enabled = false;
        r.scrub.enabled = false;
        r.scrub.targets.clear();
        r.boot.enabled = false;
        r.restore.allowed_roots = RECOVERY_ALLOWED_ROOTS
            .iter()
            .map(|s| s.to_string())
            .collect();
        Ok(r)
    }

    /// The `[[target]]` labels a scrub pass covers, in the order it scrubs
    /// them: `[scrub].targets` when it lists any, else every configured
    /// `[[target]]` in config order (bd DAS-Backup-Manager-dis3 — no built-in
    /// label list). A recovery drive's own derived config scrubs nothing: its
    /// targets are the host's, scrubbed by the host.
    pub fn scrub_target_labels(&self) -> Vec<String> {
        if self.general.recovery_drive {
            Vec::new()
        } else if self.scrub.targets.is_empty() {
            self.targets.iter().map(|t| t.label.clone()).collect()
        } else {
            self.scrub.targets.clone()
        }
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

        // btrbk would be told nothing (the renderer refuses it) and expiry
        // would keep the target's retired series forever (bd iyq).
        for tgt in &self.targets {
            let r = &tgt.retention;
            if r.daily == 0 && r.weekly == 0 && r.monthly == 0 && r.yearly == 0 {
                errors.push(crate::btrbk_conf::retention_all_zero(&tgt.label));
            }
        }

        if self.doctor.max_snapshot_age_days == 0 {
            errors.push(
                "doctor.max_snapshot_age_days must be at least 1 (0 would mark every pair \
                 undelivered)"
                    .into(),
            );
        }
        if self.recovery_os.max_age_days == 0 {
            errors.push(
                "recovery_os.max_age_days must be at least 1 (0 would mark every recovery OS stale)"
                    .into(),
            );
        }
        if let Some(org) = &self.recovery_os.egress_org
            && !egress_org_is_valid(org)
        {
            errors.push(format!(
                "recovery_os.egress_org {org:?} must be letters, digits and spaces, starting with a letter or digit (an AS number: AS64500)"
            ));
        }
        if let Some(dir) = &self.recovery_os.local_docs_dir
            && !local_docs_dir_is_valid(dir)
        {
            errors.push(format!(
                "recovery_os.local_docs_dir {dir:?} must be an absolute path of letters, digits, . _ - and /, with no . or .. component"
            ));
        }
        // A listed scrub target no [[target]] defines is deliberately NOT a
        // validation error: configs saved by older versions carry the old
        // built-in label list, and validate() gates `subvol sync` (every
        // backup run), adoption and the GUI's config_set — refusing here would
        // fail every nightly backup on an install whose targets are named
        // differently. The scrub pass reports an unresolved label instead
        // (bd DAS-Backup-Manager-dis3).
        if let Some(user) = &self.recovery_os.install_user
            && !install_user_is_valid(user)
        {
            errors.push(format!(
                "recovery_os.install_user {user:?} must be a user name: a lower-case letter or _, then up to 31 of a-z 0-9 _ -, and not root"
            ));
        }

        // One volume under two spellings ('/vol' and '/vol/', '/vol/./x') is
        // two volumes to sync, which groups by the string: every subvolume was
        // adopted and sent twice (bd sp78.13). Sources may share a volume, so
        // only different strings naming one path are refused. Compared as the
        // expiry guard compares directories (`btrbk_conf::normalise_dir`, bd
        // 31q). A symlinked spelling needs the filesystem and is not judged here.
        let mut spellings: Vec<(&str, String)> = Vec::new();
        for source in &self.sources {
            let normal = crate::btrbk_conf::normalise_dir(&source.volume);
            if let Some((other, _)) = spellings
                .iter()
                .find(|(raw, n)| *n == normal && *raw != source.volume.as_str())
            {
                errors.push(format!(
                    "volumes '{other}' and '{}' are one path spelled two ways: write it the same \
                     in every [[source]], or each subvolume is backed up twice",
                    source.volume
                ));
            } else if !spellings
                .iter()
                .any(|(raw, _)| *raw == source.volume.as_str())
            {
                spellings.push((source.volume.as_str(), normal));
            }
        }

        // A label no [[target]] has renders the source with no target line:
        // nothing of it is backed up, without a word (bd sp78.8). Unlike the
        // scrub's labels, target_labels has no built-in default an older
        // config could carry, so such a label is a typo.
        for source in &self.sources {
            for label in &source.target_labels {
                if !self.targets.iter().any(|t| &t.label == label) {
                    errors.push(format!(
                        "source '{}': target_labels names '{label}', which no [[target]] has — \
                         nothing of this source would be backed up there",
                        source.label
                    ));
                }
            }
        }

        // A recovery drive's own OS backs nothing up: it has no sources by design.
        if self.sources.is_empty() && !self.general.recovery_drive {
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

        errors.extend(self.boot_input_errors());
        errors.extend(self.subvolume_name_errors());

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
            // The recovery-OS reading mounts exactly this partition (bd
            // DAS-Backup-Manager-sp78.20): only a mirror has one to read, and
            // only a PARTUUID names it — a label is renameable, a vfat UUID is
            // not unique enough to stake a mount on.
            if let Some(pu) = &tgt.esp_partuuid {
                if tgt.role != TargetRole::Mirror {
                    errors.push(format!(
                        "Target '{}' (index {i}) sets `esp_partuuid`, which only a `role = \"mirror\"` target uses",
                        tgt.label
                    ));
                } else if !is_partuuid(pu) {
                    errors.push(format!(
                        "Target '{}' (index {i}): `esp_partuuid` {pu:?} is not a PARTUUID (8-4-4-4-12 hex digits)",
                        tgt.label
                    ));
                }
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
                // btrbk refuses a name holding '/' ("Invalid file name"), and
                // with it every run (bd DAS-Backup-Manager-31q).
                if snap.contains('/') {
                    errors.push(format!(
                        "Subvolume '{}' on volume '{}' resolves to snapshot name '{snap}', \
                         which holds a '/' btrbk refuses — give it an explicit snapshot_name",
                        sv.name, src.volume
                    ));
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

    #[test]
    fn recovery_preferred_root_is_the_first_target_only_on_a_recovery_drive() {
        let cfg = Config::from_toml(&minimal_toml(ONE_TARGET)).unwrap();
        assert_eq!(cfg.recovery_preferred_root(), None);
        let mut rec = cfg.clone();
        rec.general.recovery_drive = true;
        assert_eq!(rec.recovery_preferred_root().as_deref(), Some("/mnt/t"));
        rec.targets.clear();
        assert_eq!(rec.recovery_preferred_root(), None);
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
    fn after_index_round_trips_and_defaults_false() {
        // Adapted from the plan's `parse_sources_fixture`: the module's own
        // `minimal_toml` + `source_with` helpers build the document.
        let extra = format!(
            "{}[[source.subvolumes]]\nname = \"@/var/lib/das-backup\"\nsnapshot_name = \"das-state\"\nafter_index = true\n{ONE_TARGET}",
            source_with("s", "/vol", &[("a", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        assert!(!cfg.sources[0].subvolumes[0].after_index);
        assert!(cfg.sources[0].subvolumes[1].after_index);
        let out = cfg.to_toml().unwrap();
        assert_eq!(out.matches("after_index").count(), 1, "{out}");
        assert!(out.contains("after_index = true"), "{out}");
        let again = Config::from_toml(&out).unwrap();
        assert!(again.sources[0].subvolumes[1].after_index);
        assert!(!again.sources[0].subvolumes[0].after_index);
    }

    #[test]
    fn a_bare_string_subvolume_is_not_after_index() {
        let cfg = Config::from_toml(&minimal_toml(&format!(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\nsubvolumes = [\"@\"]\n{ONE_TARGET}"
        )))
        .unwrap();
        assert!(!cfg.sources[0].subvolumes[0].after_index);
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

    /// btrbk 0.32.7 refuses a snapshot name holding `/` ("Invalid file name"),
    /// and then every backup fails. Two unnamed entries `a/b` and `a-b`
    /// collide and the resolver separates the first as `root-a/b` (bd
    /// DAS-Backup-Manager-31q); an explicit one can say it outright.
    #[test]
    fn validate_rejects_a_snapshot_name_holding_a_slash() {
        for subvols in [
            &[("a/b", None), ("a-b", None)][..],
            &[("@data", Some("data/one"))],
        ] {
            let extra = format!("{}{ONE_TARGET}", source_with("s", "/vol", subvols));
            let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
            let errors = cfg.validate();
            assert!(
                errors
                    .iter()
                    .any(|e| e.contains("'/'") && e.contains("snapshot name")),
                "{subvols:?}: {errors:?}"
            );
        }
        // A retired sibling still takes part in the resolving, and the
        // renderer gives the live `a/b` that same `root-a/b`.
        let extra = format!(
            "{}[[source.subvolumes]]\nname = \"a-b\"\nretired = \"2026-01-01\"\n{ONE_TARGET}",
            source_with("s", "/vol", &[("a/b", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        let errors = cfg.validate();
        assert!(
            errors.iter().any(|e| e.contains("'root-a/b'")),
            "{errors:?}"
        );
        let rendered = crate::btrbk_conf::render_btrbk_conf(&cfg).unwrap();
        assert!(rendered.contains("root-a/b"), "{rendered}");
        // Flattened, as the resolver does for a lone nested subvolume.
        let extra = format!("{}{ONE_TARGET}", source_with("s", "/vol", &[("a/b", None)]));
        assert_eq!(
            Config::from_toml(&minimal_toml(&extra)).unwrap().validate(),
            Vec::<String>::new()
        );
    }

    /// A target whose every retention period is 0 keeps nothing to btrbk and
    /// everything to expiry: refused, naming it (bd DAS-Backup-Manager-iyq).
    #[test]
    fn validate_rejects_a_target_whose_retention_is_all_zero() {
        let zero = ONE_TARGET.replace("daily = 7\n", "");
        assert_ne!(zero, ONE_TARGET, "fixture must drop the only period");
        let extra = format!("{}{zero}", source_with("s", "/vol", &[("a", None)]));
        let errors = Config::from_toml(&minimal_toml(&extra)).unwrap().validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("'t'") && e.contains("retention")),
            "{errors:?}"
        );
        let extra = format!("{}{ONE_TARGET}", source_with("s", "/vol", &[("a", None)]));
        assert!(
            Config::from_toml(&minimal_toml(&extra))
                .unwrap()
                .validate()
                .is_empty()
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

    // -- the volume's top level, `.` (bd DAS-Backup-Manager-xtnq) -------------

    const MIRROR_TARGET: &str = "[[target]]\nlabel = \"m\"\nserial = \"Y\"\nmount = \"/mnt/m\"\nrole = \"mirror\"\n[target.retention]\ndaily = 7\n";

    /// `/vol` with `@` in source "s" (to every target), plus `extra` sources,
    /// a primary target "t" and a mirror "m".
    fn top_level_config(extra: &str) -> Config {
        let toml = format!(
            "{}{extra}{ONE_TARGET}{MIRROR_TARGET}",
            source_with("s", "/vol", &[("@", None)])
        );
        Config::from_toml(&minimal_toml(&toml)).unwrap()
    }

    fn primary_only(label: &str, volume: &str, subvols: &[(&str, Option<&str>)]) -> String {
        source_with(label, volume, subvols).replacen(
            "device = \"/dev/sda\"\n",
            "device = \"/dev/sda\"\ntarget_labels = [\"t\"]\n",
            1,
        )
    }

    #[test]
    fn validate_accepts_a_named_primary_only_top_level_entry() {
        let cfg = top_level_config(&primary_only(
            "s-adopted",
            "/vol",
            &[(".", Some("vol-toplevel"))],
        ));
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_refuses_a_top_level_entry_without_an_explicit_snapshot_name() {
        // It would otherwise be snapshotted as ".<timestamp>".
        let cfg = top_level_config(&primary_only("s-adopted", "/vol", &[(".", None)]));
        let errors = cfg.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("top level") && errors[0].contains("snapshot_name"),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_refuses_a_top_level_entry_that_reaches_a_mirror() {
        // In the source that sends everywhere: the recovery drive would get it.
        let cfg = top_level_config(&source_with("x", "/vol", &[(".", Some("vol-toplevel"))]));
        let errors = cfg.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("top level")
                && errors[0].contains("'x'")
                && errors[0].contains("'m'"),
            "{errors:?}"
        );
        // Named explicitly as a target: refused the same way.
        let named = source_with("x", "/vol", &[(".", Some("vol-toplevel"))]).replacen(
            "device = \"/dev/sda\"\n",
            "device = \"/dev/sda\"\ntarget_labels = [\"t\", \"m\"]\n",
            1,
        );
        assert_eq!(top_level_config(&named).validate().len(), 1);
        // A retired entry is not rendered and sends nowhere.
        let mut cfg = top_level_config(&source_with("x", "/vol", &[(".", Some("vol-toplevel"))]));
        cfg.sources[1].subvolumes[0].retired = Some("2026-10-01".into());
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_refuses_one_volume_spelled_two_ways() {
        // bd DAS-Backup-Manager-sp78.13: '/vol' and '/vol/' are one
        // filesystem, but sync groups by the string, so every subvolume was
        // adopted twice and sent twice.
        let two = format!(
            "{}{}{ONE_TARGET}",
            source_with("a", "/vol", &[("@a", None)]),
            source_with("b", "/vol/", &[("@b", None)])
        );
        let errors = Config::from_toml(&minimal_toml(&two)).unwrap().validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("'/vol'") && errors[0].contains("'/vol/'"),
            "{errors:?}"
        );
        // A leading './' is one more spelling (bd 31q, from 12k):
        // `Path::components` kept it, so 'vol' and './vol' passed.
        let dotted = format!(
            "{}{}{ONE_TARGET}",
            source_with("a", "vol", &[("@a", None)]),
            source_with("b", "./vol", &[("@b", None)])
        );
        let errors = Config::from_toml(&minimal_toml(&dotted))
            .unwrap()
            .validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("'vol'") && e.contains("'./vol'")),
            "{errors:?}"
        );
        // One spelling shared by two sources is how sync splits a volume: valid.
        let same = format!(
            "{}{}{ONE_TARGET}",
            source_with("a", "/vol", &[("@a", None)]),
            source_with("b", "/vol", &[("@b", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&same)).unwrap();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn a_second_spelling_is_caught_behind_another_volume() {
        // bd DAS-Backup-Manager-s5jv.3: each volume's first spelling is
        // remembered, not only the first volume's.
        let three = format!(
            "{}{}{}{ONE_TARGET}",
            source_with("o", "/other", &[("@o", None)]),
            source_with("a", "/vol", &[("@a", None)]),
            source_with("b", "/vol/", &[("@b", None)])
        );
        let errors = Config::from_toml(&minimal_toml(&three)).unwrap().validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("'/vol'") && errors[0].contains("'/vol/'"),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_refuses_a_target_label_no_target_has() {
        // bd DAS-Backup-Manager-sp78.8: a typo renders the source with no
        // target line, so nothing of it is backed up, and validate passed.
        let typo = source_with("x", "/vol", &[("@a", None)]).replacen(
            "device = \"/dev/sda\"\n",
            "device = \"/dev/sda\"\ntarget_labels = [\"t\", \"primray\"]\n",
            1,
        );
        let cfg = Config::from_toml(&minimal_toml(&format!("{typo}{ONE_TARGET}"))).unwrap();
        let errors = cfg.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("'x'") && errors[0].contains("'primray'"),
            "{errors:?}"
        );
        // Every label a target has: valid.
        let named = source_with("x", "/vol", &[("@a", None)]).replacen(
            "device = \"/dev/sda\"\n",
            "device = \"/dev/sda\"\ntarget_labels = [\"t\"]\n",
            1,
        );
        let cfg = Config::from_toml(&minimal_toml(&format!("{named}{ONE_TARGET}"))).unwrap();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_refuses_a_volume_whose_top_level_is_configured_twice() {
        let cfg = top_level_config(&format!(
            "{}{}",
            primary_only("a", "/vol", &[(".", Some("one"))]),
            primary_only("b", "/vol", &[(".", Some("two"))])
        ));
        let errors = cfg.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("/vol") && errors[0].contains("more than once"),
            "{errors:?}"
        );
        // Two volumes, one top level each: fine.
        let cfg = top_level_config(&format!(
            "{}{}",
            primary_only("a", "/vol", &[(".", Some("one"))]),
            primary_only("b", "/other", &[(".", Some("two"))])
        ));
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_refuses_dot_components_in_any_other_entry_name() {
        for bad in ["./a", "a/./b", "..", "a/..", "../x", "a//b", "/a", "a/", ""] {
            let cfg = top_level_config(&primary_only("x", "/vol", &[(bad, Some("n"))]));
            let errors = cfg.validate();
            assert_eq!(errors.len(), 1, "{bad:?}: {errors:?}");
            assert!(
                errors[0].contains(&format!("'{bad}'")),
                "{bad:?}: {errors:?}"
            );
        }
        // ('@data', not '@': the fixture's first source lists '@', a boot
        // subvolume, and a second source may not: bd 5237.12.)
        for ok in [".hidden", "..x", "a.b/c", "@data"] {
            let cfg = top_level_config(&primary_only("x", "/vol", &[(ok, Some("n"))]));
            assert!(cfg.validate().is_empty(), "{ok}: {:?}", cfg.validate());
        }
    }

    /// Review of bc2c8a4: `snapshot_dir` is relative to the volume. Empty or
    /// absolute, the snapshot step's filter becomes `<volume>//<name>`.
    #[test]
    fn validate_refuses_an_empty_or_absolute_snapshot_dir() {
        // And, re-review of b62ddbc: nothing that leaves the volume or
        // yields `<volume>/./<name>`.
        for bad in [
            "", "/snaps", "/", ".", "..", "./", "../x", "a/../b", "a/..", "./snaps", "a/./b",
            "a//b",
        ] {
            let mut cfg = top_level_config("");
            cfg.sources[0].snapshot_dir = bad.into();
            let errors = cfg.validate();
            assert_eq!(errors.len(), 1, "{bad:?}: {errors:?}");
            assert!(
                errors[0].contains("snapshot_dir") && errors[0].contains("'s'"),
                "{bad:?}: {errors:?}"
            );
        }
        for ok in [
            ".btrbk-snapshots",
            "ClaudeCodeProjects/.btrbk-snapshots",
            "snaps/",
            "..snaps",
            "a..b/c",
        ] {
            let mut cfg = top_level_config("");
            cfg.sources[0].snapshot_dir = ok.into();
            assert!(cfg.validate().is_empty(), "{ok}: {:?}", cfg.validate());
        }
    }

    #[test]
    fn validate_refuses_an_exclude_pattern_that_can_match_nothing() {
        // "" matches no subvolume and "." is not how the top level is
        // excluded ("/" is): both would be accepted and ignored.
        for bad in ["", "."] {
            let mut cfg = top_level_config("");
            cfg.subvolumes.exclude = vec![bad.into()];
            let errors = cfg.validate();
            assert_eq!(errors.len(), 1, "{bad:?}: {errors:?}");
            assert!(errors[0].contains("exclude"), "{bad:?}: {errors:?}");
            // The older [doctor] key is merged and checked the same way.
            let mut cfg = top_level_config("");
            cfg.doctor.exclude = vec![bad.into()];
            assert_eq!(cfg.validate().len(), 1, "{bad:?}");
        }
        let mut cfg = top_level_config("");
        cfg.subvolumes.exclude = vec!["/".into(), "@cache".into(), "*.tmp".into()];
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_ignores_a_retired_entry_when_checking_snapshot_names() {
        // `@x` and the retired `x` both resolve to `root-x`: no clash, since a
        // retired entry is not rendered. (With `a/b` beside a retired `a-b`
        // the live one resolves to `root-a/b`, which btrbk refuses: bd 31q,
        // validate_rejects_a_snapshot_name_holding_a_slash.)
        let extra = format!(
            "{}[[source.subvolumes]]\nname = \"x\"\nretired = \"2026-01-01\"\n{ONE_TARGET}",
            source_with("s", "/vol", &[("@x", None)])
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
        let second = ONE_TARGET
            .replace("serial = \"X\"", "serial = \"Y\"")
            .replace("/mnt/t", "/mnt/u");
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
    fn validate_accepts_an_esp_partuuid_only_on_a_mirror_and_only_as_a_partuuid() {
        let with = |role: &str, key: &str| {
            let target = ONE_TARGET
                .replace("role = \"primary\"", &format!("role = \"{role}\""))
                .replace("mount = ", &format!("{key}mount = "));
            let extra = format!("{}{target}", source_with("s", "/vol", &[("a", None)]));
            Config::from_toml(&minimal_toml(&extra)).unwrap()
        };
        let good = "esp_partuuid = \"5eed0025-0000-4000-8000-000000000025\"\n";
        let cfg = with("mirror", good);
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        assert_eq!(
            cfg.targets[0].esp_partuuid.as_deref(),
            Some("5eed0025-0000-4000-8000-000000000025")
        );
        // Written back only when set.
        let written = toml::to_string(&cfg).unwrap();
        assert!(written.contains("esp_partuuid"), "{written}");
        assert!(
            !toml::to_string(&with("mirror", ""))
                .unwrap()
                .contains("esp_partuuid")
        );

        // On a primary it would be accepted and ignored: refused.
        let errors = with("primary", good).validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("esp_partuuid") && errors[0].contains("mirror"),
            "{errors:?}"
        );

        // Not a PARTUUID: a label, a vfat UUID, a path — refused.
        for bad in [
            "RECOV-ESP-1",
            "ABCD-0003",
            "/dev/sdb1",
            "5eed0025-0000-4000-8000-00000000002",
            "",
        ] {
            let errors = with("mirror", &format!("esp_partuuid = \"{bad}\"\n")).validate();
            assert_eq!(errors.len(), 1, "{bad}: {errors:?}");
            assert!(errors[0].contains("esp_partuuid"), "{bad}: {errors:?}");
        }
    }

    #[test]
    fn validate_rejects_two_targets_sharing_a_mount_and_dot_boot_components() {
        let second = ONE_TARGET
            .replace("serial = \"X\"", "serial = \"Y\"")
            .replace("label = \"t\"", "label = \"u\"");
        let extra = format!(
            "{}{ONE_TARGET}{second}",
            source_with("s", "/vol", &[("a", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        let errors = cfg.boot_input_errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("mount '/mnt/t'"), "{errors:?}");
        assert_eq!(cfg.validate(), errors);
        // One mount spelled two ways is still one mount (bd 2jkw item 2).
        let slashed = second.replace("mount = \"/mnt/t\"", "mount = \"/mnt/t/\"");
        assert_ne!(slashed, second, "fixture must spell the mount differently");
        let extra = format!(
            "{}{ONE_TARGET}{slashed}",
            source_with("s", "/vol", &[("a", None)])
        );
        let errors = Config::from_toml(&minimal_toml(&extra))
            .unwrap()
            .boot_input_errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        for bad in [".", "..", "a/../b", "./a"] {
            let extra = format!("{}{ONE_TARGET}", source_with("s", "/vol", &[("a", None)]));
            let mut cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
            cfg.boot.subvolumes = vec![bad.into()];
            let errors = cfg.boot_input_errors();
            assert_eq!(errors.len(), 1, "{bad}: {errors:?}");
            assert!(errors[0].contains("'.' or '..'"), "{bad}: {errors:?}");
        }
        for ok in ["@", "@home", "..x", ".hidden"] {
            let extra = format!("{}{ONE_TARGET}", source_with("s", "/vol", &[("a", None)]));
            let mut cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
            cfg.boot.subvolumes = vec![ok.into()];
            assert!(cfg.boot_input_errors().is_empty(), "{ok}");
        }
    }

    #[test]
    fn a_boot_subvolume_listed_by_two_sources_is_refused() {
        // Each source's series lives in its own target directory; the boot
        // step would take the newest snapshot across both, another volume's OS
        // among them (bd DAS-Backup-Manager-5237.12).
        let two = format!(
            "{}{}{ONE_TARGET}",
            source_with("nvme-root", "/vol-a", &[("@", None), ("@home", None)]),
            source_with("sata-os", "/vol-b", &[("@", None), ("data", None)])
        );
        let mut cfg = Config::from_toml(&minimal_toml(&two)).unwrap();
        cfg.boot.subvolumes = vec!["@".into(), "@home".into()];
        let errors = cfg.boot_input_errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("'@'")
                && errors[0].contains("nvme-root")
                && errors[0].contains("sata-os"),
            "{errors:?}"
        );
        assert!(cfg.validate().contains(&errors[0]));
        // A retired entry's series is still on the targets: it counts.
        let retired = two.replace(
            "volume = \"/vol-b\"\ndevice = \"/dev/sda\"\n[[source.subvolumes]]\nname = \"@\"\n",
            "volume = \"/vol-b\"\ndevice = \"/dev/sda\"\n[[source.subvolumes]]\nname = \"@\"\nretired = \"2026-11-01\"\n",
        );
        assert_ne!(retired, two, "fixture must retire the second entry");
        let mut cfg = Config::from_toml(&minimal_toml(&retired)).unwrap();
        cfg.boot.subvolumes = vec!["@".into()];
        assert_eq!(cfg.boot_input_errors().len(), 1);
        // The same name in two sources is fine when it is not a boot subvolume,
        // and one source listing each boot subvolume is the ordinary case.
        cfg.boot.subvolumes = vec!["@home".into()];
        assert!(cfg.boot_input_errors().is_empty());
        cfg.boot.subvolumes = vec!["data".into()];
        assert!(cfg.boot_input_errors().is_empty());
    }

    #[test]
    fn save_writes_a_missing_file_fresh_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::from_toml(&minimal_toml("")).unwrap();
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(FRESH_HEADER), "{text}");
        // The header says what is true: edits are kept (bd m0pr.1).
        assert!(
            text.starts_with(
                "# Written by btrdasd setup. Your edits and comments are kept when btrdasd saves this file.\n"
            ),
            "{text}"
        );
        assert!(!text.contains("do not edit"), "{text}");
        assert_eq!(
            Config::from_toml(&text).unwrap().to_toml().unwrap(),
            cfg.to_toml().unwrap()
        );
        // Edited in place on the next save: no temp file either way.
        cfg.schedule.full = "Sat 05:00".into();
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(FRESH_HEADER), "{text}");
        assert!(text.contains("full = \"Sat 05:00\""), "{text}");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["config.toml"]);
    }

    /// With nothing known about the file, regenerating it would erase what it
    /// holds: refused, the file as it was, the error naming it once.
    #[test]
    fn save_refuses_a_file_it_cannot_read_as_a_config_and_leaves_it_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::from_toml(&minimal_toml("")).unwrap();
        let path = dir.path().join("config.toml");
        for (old, why) in [
            ("old contents [ that are not TOML", "is not TOML"),
            (
                "[general]\nfuture_only = 1\n",
                "is not a config this version can read",
            ),
        ] {
            std::fs::write(&path, old).unwrap();
            let err = cfg.save(&path).unwrap_err().to_string();
            assert!(
                err.starts_with(&format!("cannot write {}: ", path.display())),
                "{err}"
            );
            assert!(err.contains(why), "{err}");
            assert!(err.contains("left unchanged"), "{err}");
            assert_eq!(err.matches(&path.display().to_string()).count(), 1, "{err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        }
        // Present but unreadable (a directory): refused, not written fresh.
        let path = dir.path().join("a-directory.toml");
        std::fs::create_dir(&path).unwrap();
        let err = cfg.save(&path).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert_eq!(err.matches(&path.display().to_string()).count(), 1, "{err}");
        assert!(path.is_dir());
        // Present but unreadable as text (not UTF-8), in a directory the save
        // could write: only a MISSING file is written fresh.
        let path = dir.path().join("not-utf8.toml");
        std::fs::write(&path, b"[general]\nversion = \"\xff\"\n").unwrap();
        let err = cfg.save(&path).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"[general]\nversion = \"\xff\"\n"
        );
    }

    #[test]
    fn save_keeps_the_files_mode_and_a_failure_names_the_file_once() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::from_toml(&minimal_toml("")).unwrap();
        // The reviewer's probe: a private config.toml stays private.
        let path = dir.path().join("config.toml");
        std::fs::write(&path, minimal_toml("")).unwrap();
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
        assert_eq!(parsed.general.version, crate::VERSION);
        assert_eq!(parsed.init.system, InitSystem::Systemd);
        assert_eq!(parsed.schedule.incremental, "03:00");
        assert_eq!(parsed.schedule.full, "Sun 04:00");
        assert_eq!(parsed.schedule.randomized_delay_min, 30);
        // New fields have defaults
        assert_eq!(parsed.general.log_file, "/var/log/das-backup.log");
        assert_eq!(parsed.general.btrbk_conf, "/etc/btrbk/btrbk.conf");
        assert!(parsed.das.model_pattern.is_empty());
        assert_eq!(parsed.das.io_scheduler, "mq-deadline");
        assert!(parsed.boot.enabled);
        assert_eq!(parsed.boot.subvolumes, vec!["@", "@home"]);
        assert_eq!(parsed.boot.archive_retention_days, 60);
        assert!(parsed.scrub.enabled);
        assert_eq!(parsed.scrub.on_calendar, "*-*-01 03:05:00");
        // No built-in labels: empty means every [[target]] (bd dis3).
        assert!(parsed.scrub.targets.is_empty());
        assert_eq!(parsed.scrub.warn_age_days, 45);
        assert_eq!(parsed.scrub.fail_age_days, 75);
        // [doctor] is optional — default config has no exclude patterns
        assert!(parsed.doctor.exclude.is_empty());
    }

    /// `[recovery_os].local_docs_dir` (bd DAS-Backup-Manager-5237.6): absent,
    /// the directory beside the config's own, `<config dir>-docs`; set, an
    /// absolute plain path outside the config's directory. live-sync versions
    /// the config's directory, and refuses one that holds a subdirectory.
    #[test]
    fn local_docs_dir_defaults_beside_the_config_directory_and_never_inside_it() {
        let conf = Path::new("/etc/das-backup/config.toml");
        let mut cfg = Config::from_toml(&minimal_toml("")).unwrap();
        assert_eq!(
            local_docs_dir(&cfg, conf),
            Ok(PathBuf::from("/etc/das-backup-docs"))
        );
        assert_eq!(
            legacy_local_docs_dir(conf),
            PathBuf::from("/etc/das-backup/docs")
        );
        cfg.recovery_os.local_docs_dir = Some("/srv/recovery-docs".into());
        assert_eq!(
            local_docs_dir(&cfg, conf),
            Ok(PathBuf::from("/srv/recovery-docs"))
        );
        for inside in [
            "/etc/das-backup",
            "/etc/das-backup/docs",
            "/etc/das-backup/x/y",
        ] {
            cfg.recovery_os.local_docs_dir = Some(inside.into());
            let err = local_docs_dir(&cfg, conf).unwrap_err();
            assert!(
                err.contains("inside") && err.contains(inside),
                "{inside}: {err}"
            );
        }
        // A config at the root has no directory to put one beside.
        cfg.recovery_os.local_docs_dir = None;
        assert!(local_docs_dir(&cfg, Path::new("/config.toml")).is_err());
        let text = minimal_toml("[recovery_os]\nlocal_docs_dir = \"/srv/d\"\n");
        let cfg = Config::from_toml(&text).unwrap();
        assert_eq!(cfg.recovery_os.local_docs_dir.as_deref(), Some("/srv/d"));
        assert!(
            cfg.to_toml()
                .unwrap()
                .contains("local_docs_dir = \"/srv/d\"\n")
        );
        // Unset is never written out: the default stays the resolver's.
        assert!(
            !Config::from_toml(&minimal_toml(""))
                .unwrap()
                .to_toml()
                .unwrap()
                .contains("local_docs_dir")
        );
    }

    /// The value reaches a shell (`eval` of `dump-env`) and the installer's
    /// paths: an absolute path of letters, digits, `.`, `_`, `-` and `/` only,
    /// with no `.` or `..` component.
    #[test]
    fn local_docs_dir_must_be_a_plain_absolute_path() {
        for good in ["/etc/das-backup-docs", "/srv/a_b/c-d.e"] {
            assert!(local_docs_dir_is_valid(good), "{good}");
        }
        for bad in [
            "",
            "relative/docs",
            "/",
            "/etc/../x",
            "/etc/./x",
            "/a b",
            "/a'b",
            "/a$b",
            "/a\nb",
            "/é",
        ] {
            assert!(!local_docs_dir_is_valid(bad), "{bad:?}");
            let mut cfg = Config::from_toml(&minimal_toml("")).unwrap();
            cfg.recovery_os.local_docs_dir = Some(bad.into());
            assert!(
                cfg.validate()
                    .iter()
                    .any(|e| e.contains("recovery_os.local_docs_dir")),
                "{bad:?}: {:?}",
                cfg.validate()
            );
        }
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
    fn doctor_snapshot_age_defaults_to_three_days_and_round_trips() {
        let cfg = Config::from_toml(&minimal_toml("")).unwrap();
        assert_eq!(cfg.doctor.max_snapshot_age_days, 3, "absent section");
        assert_eq!(Doctor::default().max_snapshot_age_days, 3);
        let cfg = Config::from_toml(&minimal_toml("[doctor]\nexclude = [\"@x\"]\n")).unwrap();
        assert_eq!(cfg.doctor.max_snapshot_age_days, 3, "section without it");
        let cfg =
            Config::from_toml(&minimal_toml("[doctor]\nmax_snapshot_age_days = 10\n")).unwrap();
        assert_eq!(cfg.doctor.max_snapshot_age_days, 10);
        let text = cfg.to_toml().unwrap();
        assert!(text.contains("max_snapshot_age_days = 10\n"), "{text}");
        assert_eq!(
            Config::from_toml(&text)
                .unwrap()
                .doctor
                .max_snapshot_age_days,
            10
        );
        assert!(
            Config::from_toml(&minimal_toml("[doctor]\nmax_snapshot_age_days = -1\n")).is_err()
        );
    }

    #[test]
    fn validate_requires_a_doctor_snapshot_age_of_at_least_one_day() {
        let msg = "doctor.max_snapshot_age_days must be at least 1 (0 would mark every pair \
                   undelivered)";
        let mut cfg =
            Config::from_toml(&minimal_toml("[doctor]\nmax_snapshot_age_days = 0\n")).unwrap();
        assert!(
            cfg.validate().iter().any(|e| e == msg),
            "{:?}",
            cfg.validate()
        );
        cfg.doctor.max_snapshot_age_days = 1;
        assert!(
            !cfg.validate()
                .iter()
                .any(|e| e.contains("max_snapshot_age_days")),
            "{:?}",
            cfg.validate()
        );
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

    /// Unset is "not configured" and stays out of the file; a value round-trips
    /// (bd DAS-Backup-Manager-6pd3: there is no built-in ISP).
    #[test]
    fn recovery_os_egress_org_is_unset_by_default_and_round_trips() {
        let cfg = Config::from_toml(&minimal_toml("")).unwrap();
        assert_eq!(cfg.recovery_os.egress_org, None);
        assert!(!cfg.to_toml().unwrap().contains("egress_org"));
        let cfg =
            Config::from_toml(&minimal_toml("[recovery_os]\negress_org = \"AS64500\"\n")).unwrap();
        assert_eq!(cfg.recovery_os.egress_org.as_deref(), Some("AS64500"));
        let text = cfg.to_toml().unwrap();
        assert!(text.contains("egress_org = \"AS64500\""), "{text}");
        assert_eq!(
            Config::from_toml(&text)
                .unwrap()
                .recovery_os
                .egress_org
                .as_deref(),
            Some("AS64500")
        );
    }

    #[test]
    fn validate_refuses_an_egress_org_the_script_would_refuse() {
        let mut cfg = Config::from_toml(&minimal_toml("")).unwrap();
        for good in ["AS64500", "AS64500 Example ISP", "7"] {
            cfg.recovery_os.egress_org = Some(good.into());
            assert!(
                !cfg.validate().iter().any(|e| e.contains("egress_org")),
                "{good}: {:?}",
                cfg.validate()
            );
        }
        for bad in [
            "",
            " AS64500",
            "AS64500;reboot",
            "AS64500'",
            "AS\u{e9}",
            "AS64500\n",
        ] {
            cfg.recovery_os.egress_org = Some(bad.into());
            assert!(
                cfg.validate()
                    .iter()
                    .any(|e| e.contains("recovery_os.egress_org")),
                "{bad:?} accepted: {:?}",
                cfg.validate()
            );
        }
    }

    /// Unset is "not configured" and stays out of the file; a value round-trips
    /// (production separation: there is no built-in recovery-OS account).
    #[test]
    fn recovery_os_install_user_is_unset_by_default_and_round_trips() {
        let cfg = Config::from_toml(&minimal_toml("")).unwrap();
        assert_eq!(cfg.recovery_os.install_user, None);
        assert!(!cfg.to_toml().unwrap().contains("install_user"));
        let cfg = Config::from_toml(&minimal_toml(
            "[recovery_os]\ninstall_user = \"recovery\"\n",
        ))
        .unwrap();
        assert_eq!(cfg.recovery_os.install_user.as_deref(), Some("recovery"));
        let text = cfg.to_toml().unwrap();
        assert!(text.contains("install_user = \"recovery\""), "{text}");
        assert_eq!(
            Config::from_toml(&text)
                .unwrap()
                .recovery_os
                .install_user
                .as_deref(),
            Some("recovery")
        );
    }

    #[test]
    fn validate_refuses_an_install_user_the_installer_would_refuse() {
        let mut cfg = Config::from_toml(&minimal_toml("")).unwrap();
        let longest = format!("a{}", "b".repeat(31));
        for good in ["recovery", "_ops", "ops-2", "a", longest.as_str()] {
            cfg.recovery_os.install_user = Some(good.into());
            assert!(
                !cfg.validate().iter().any(|e| e.contains("install_user")),
                "{good}: {:?}",
                cfg.validate()
            );
        }
        let too_long = format!("a{}", "b".repeat(32));
        for bad in [
            "",
            "root",
            "Recovery",
            "1user",
            "-user",
            "a b",
            "a;reboot",
            "us$er",
            "r\u{e9}cup",
            "user\n",
            too_long.as_str(),
        ] {
            cfg.recovery_os.install_user = Some(bad.into());
            assert!(
                cfg.validate()
                    .iter()
                    .any(|e| e.contains("recovery_os.install_user")),
                "{bad:?} accepted: {:?}",
                cfg.validate()
            );
        }
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
        // No built-in labels: empty means every [[target]] (bd dis3).
        assert!(scrub.targets.is_empty());
        assert_eq!(scrub.warn_age_days, 45);
        assert_eq!(scrub.fail_age_days, 75);
    }

    fn scrub_target(label: &str) -> Target {
        Target {
            label: label.into(),
            serial: String::new(),
            serials: Vec::new(),
            mount_uuid: None,
            mount: format!("/mnt/backup-{label}"),
            role: TargetRole::Primary,
            retention: Retention::default(),
            display_name: String::new(),
            esp_partuuid: None,
        }
    }

    /// bd DAS-Backup-Manager-dis3: the scrub covers the targets this config
    /// names, never a built-in list — empty `[scrub].targets` is every
    /// `[[target]]` in config order; a list is that list; a recovery drive's
    /// own config scrubs nothing.
    #[test]
    fn scrub_target_labels_default_to_every_configured_target() {
        let mut cfg = Config::default();
        assert!(cfg.scrub_target_labels().is_empty());
        cfg.targets = vec![scrub_target("big"), scrub_target("small")];
        assert_eq!(cfg.scrub_target_labels(), ["big", "small"]);
        cfg.scrub.targets = vec!["small".into()];
        assert_eq!(cfg.scrub_target_labels(), ["small"]);
        cfg.scrub.targets.clear();
        cfg.general.recovery_drive = true;
        assert!(cfg.scrub_target_labels().is_empty());
    }

    /// A config saved by an older version lists the old built-in scrub labels
    /// beside targets named differently. validate() gates `subvol sync` (every
    /// backup run), adoption and the GUI's config_set, so an unknown scrub label
    /// must never be a validation error there: it would fail every nightly
    /// backup. The scrub pass reports it unresolved instead (bd dis3).
    #[test]
    fn validate_accepts_a_scrub_target_no_target_defines() {
        let mut cfg = Config::from_toml(&minimal_toml("")).unwrap();
        cfg.targets = vec![scrub_target("primary")];
        cfg.scrub.targets = vec![
            "primary-22tb".into(),
            "system-recovery-A-2tb".into(),
            "system-recovery-B-2tb".into(),
        ];
        let errs = cfg.validate();
        assert!(
            !errs.iter().any(|e| e.contains("scrub")),
            "an unknown scrub label refused the config: {errs:?}"
        );
        // ...and the labels the pass will try are exactly the listed ones, so
        // its own "unresolved" report names them.
        assert_eq!(cfg.scrub_target_labels(), cfg.scrub.targets);
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
                    snapshot_name: None,
                    ..Default::default()
                },
                SubvolConfig {
                    name: "@home".into(),
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
            serial: "SERIAL08".into(),
            serials: vec!["SERIAL08".into()],
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
            esp_partuuid: None,
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
        assert_eq!(parsed.sources[0].snapshot_dir, ".snapshots");
        assert_eq!(parsed.sources[0].target_subdirs, vec!["nvme"]);
        assert_eq!(parsed.targets.len(), 1);
        assert_eq!(parsed.targets[0].serial, "SERIAL08");
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
        assert!(cfg.das.model_pattern.is_empty());
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
        // No built-in labels: empty means every [[target]] (bd dis3).
        assert!(cfg.scrub.targets.is_empty());
        assert_eq!(cfg.scrub.warn_age_days, 45);
        assert_eq!(cfg.scrub.fail_age_days, 75);
        // [doctor] absent entirely from this old config — must parse to defaults
        assert!(cfg.doctor.exclude.is_empty());
    }

    #[test]
    fn target_parses_legacy_single_serial() {
        let toml = r#"
label = "primary"
serial = "SERIAL08"
mount = "/mnt/backup"
role = "primary"
[retention]
daily = 7
"#;
        let t: Target = toml::from_str(toml).expect("legacy form should parse");
        assert_eq!(t.serial, "SERIAL08");
        assert_eq!(t.serials, vec!["SERIAL08"]);
        assert!(t.mount_uuid.is_none());
        assert_eq!(t.effective_serials(), vec!["SERIAL08"]);
    }

    #[test]
    fn target_parses_new_serials_list() {
        let toml = r#"
label = "primary"
serials = ["SERIAL08", "SERIAL11"]
mount_uuid = "5eed0007-0000-4000-8000-000000000007"
mount = "/mnt/backup"
role = "primary"
[retention]
daily = 7
"#;
        let t: Target = toml::from_str(toml).expect("new form should parse");
        assert_eq!(t.serials, vec!["SERIAL08", "SERIAL11"]);
        // legacy `serial` is auto-populated from serials[0] for back-compat
        assert_eq!(t.serial, "SERIAL08");
        assert_eq!(
            t.mount_uuid.as_deref(),
            Some("5eed0007-0000-4000-8000-000000000007")
        );
    }

    #[test]
    fn target_parses_uuid_only() {
        // Valid: UUID-only target with no serial — degraded RAID-1 with both
        // legs unidentified by serial but the FS is mountable.
        let toml = r#"
label = "primary"
mount_uuid = "5eed0007-0000-4000-8000-000000000007"
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
            Some("5eed0007-0000-4000-8000-000000000007")
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
            serial: "SERIAL11".into(),
            serials: vec!["SERIAL11".into(), "NEWLEG".into()],
            mount_uuid: Some("5eed0007-0000-4000-8000-000000000007".into()),
            mount: "/mnt/backup-22tb".into(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 4,
                monthly: 12,
                daily: 7,
                yearly: 1,
            },
            display_name: "22TB RAID-1 Primary".into(),
            esp_partuuid: None,
        };
        let s = toml::to_string(&original).expect("serialize");
        // Legacy `serial = ` line MUST NOT appear in canonical output
        assert!(!s.contains("\nserial = "), "found legacy serial in: {s}");
        assert!(s.contains("serials = "));
        assert!(s.contains("mount_uuid = "));
        let parsed: Target = toml::from_str(&s).expect("round-trip parse");
        assert_eq!(parsed.serials, original.serials);
        assert_eq!(parsed.mount_uuid, original.mount_uuid);
        assert_eq!(parsed.serial, "SERIAL11"); // re-derived from serials[0]
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
            mount_uuid: Some("5eed0007-0000-4000-8000-000000000007".into()),
            mount: "/mnt/backup".into(),
            role: TargetRole::Primary,
            retention: Retention {
                daily: 7,
                ..Retention::default()
            },
            display_name: String::new(),
            esp_partuuid: None,
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
            esp_partuuid: None,
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
db_path = "/nonexistent/das-backup-tests/index.db"
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
    }

    #[test]
    fn subvol_config_full_format() {
        let toml = r#"
[general]
version = "0.6.0"
install_prefix = "/usr"
db_path = "/nonexistent/das-backup-tests/index.db"
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
snapshot_name = "root"
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
        assert_eq!(cfg.sources[0].subvolumes[0].snapshot_name, None);
        assert_eq!(cfg.sources[0].subvolumes[1].name, "@root");
        assert_eq!(
            cfg.sources[0].subvolumes[1].snapshot_name.as_deref(),
            Some("root")
        );
    }

    /// Minimal config carrying only the mandatory sections, plus whatever
    /// optional sections a test appends. Parsing real TOML (rather than calling
    /// the `default_*` fns) is the point: it exercises the `#[serde(default)]`
    /// wiring an on-disk config.toml actually goes through.
    const BOOTED_B: &str = "5eed0010-0000-4000-8000-000000000010";

    fn test_config_with_three_targets() -> Config {
        let target = |label: &str, uuid: &str, mount: &str, role: &str| {
            format!(
                "[[target]]\nlabel = \"{label}\"\nserial = \"S-{label}\"\nmount_uuid = \"{uuid}\"\nmount = \"{mount}\"\nrole = \"{role}\"\n[target.retention]\ndaily = 7\n"
            )
        };
        let extra = format!(
            "{}{}{}{}[scrub]\nenabled = true\ntargets = [\"primary-22tb\"]\n",
            source_with("s", "/vol", &[("a", None)]),
            target(
                "primary-22tb",
                "5eed0016-0000-4000-8000-000000000016",
                "/mnt/backup-22tb",
                "primary"
            ),
            target(
                "system-recovery-A-2tb",
                "5eed0008-0000-4000-8000-000000000008",
                "/mnt/backup-system-recovery-A",
                "mirror"
            ),
            target(
                "system-recovery-B-2tb",
                BOOTED_B,
                "/mnt/backup-system-recovery-B",
                "mirror"
            ),
        );
        let mut cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        cfg.email.enabled = true;
        cfg
    }

    #[test]
    fn derive_recovery_keeps_every_target_with_the_booted_one_first_and_drops_the_sources() {
        let cfg = test_config_with_three_targets();
        let r = cfg.derive_recovery(BOOTED_B).unwrap();
        assert!(r.general.recovery_drive);
        assert!(r.sources.is_empty());
        let labels: Vec<&str> = r.targets.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "system-recovery-B-2tb",
                "primary-22tb",
                "system-recovery-A-2tb"
            ]
        );
        assert_eq!(r.targets[0].mount, "/mnt/backup-system-recovery-B");
        assert_eq!(r.targets[1].mount, "/mnt/backup-22tb");
        assert!(!r.email.enabled);
        assert!(!r.scrub.enabled);
        assert!(r.scrub.targets.is_empty());
        assert!(!r.boot.enabled);
        assert_eq!(
            r.restore.allowed_roots,
            vec!["/mnt/restore".to_string(), "/home/restore-test".to_string()]
        );
        // The input is untouched, and a host is not a recovery drive.
        assert!(!cfg.general.recovery_drive && !cfg.sources.is_empty() && cfg.email.enabled);
    }

    #[test]
    fn derive_recovery_matches_the_uuid_case_insensitively() {
        let cfg = test_config_with_three_targets();
        let r = cfg.derive_recovery(&BOOTED_B.to_uppercase()).unwrap();
        assert_eq!(r.targets[0].label, "system-recovery-B-2tb");
    }

    #[test]
    fn derive_recovery_refuses_an_unknown_uuid_naming_the_targets() {
        let cfg = test_config_with_three_targets();
        let e = cfg
            .derive_recovery("00000000-0000-0000-0000-000000000000")
            .unwrap_err();
        assert!(
            e.contains("no target of filesystem 00000000-0000-0000-0000-000000000000"),
            "{e}"
        );
        assert!(
            e.contains("primary-22tb")
                && e.contains("system-recovery-A-2tb")
                && e.contains("system-recovery-B-2tb"),
            "{e}"
        );
    }

    #[test]
    fn derive_recovery_refuses_an_ambiguous_uuid() {
        // Review focus 5: two targets with the same mount_uuid (a hand edit).
        let mut cfg = test_config_with_three_targets();
        cfg.targets[0].mount_uuid = cfg.targets[2].mount_uuid.clone();
        let e = cfg.derive_recovery(BOOTED_B).unwrap_err();
        assert!(
            e.contains("two targets")
                && e.contains("primary-22tb")
                && e.contains("system-recovery-B-2tb"),
            "{e}"
        );
    }

    #[test]
    fn derive_recovery_output_validates_and_round_trips() {
        let cfg = test_config_with_three_targets();
        let r = cfg.derive_recovery(BOOTED_B).unwrap();
        assert!(r.validate().is_empty(), "{:?}", r.validate());
        let again = Config::from_toml(&r.to_toml().unwrap()).unwrap();
        assert!(again.general.recovery_drive);
        assert_eq!(again.targets.len(), 3);
        assert_eq!(again.targets[0].label, "system-recovery-B-2tb");
    }

    #[test]
    fn validate_still_requires_a_source_when_not_a_recovery_drive() {
        let mut cfg = test_config_with_three_targets();
        cfg.sources.clear();
        assert!(
            cfg.validate()
                .iter()
                .any(|e| e.contains("No backup sources"))
        );
    }

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
db_path = "/nonexistent/das-backup-tests/index.db"
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
  { name = "@gone", snapshot_name = "gone", retired = "2026-10-02" },
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
        type Entry = (String, Option<String>, Option<String>, Option<String>);
        let entries = |c: &Config| -> Vec<Entry> {
            c.sources[0]
                .subvolumes
                .iter()
                .map(|e| {
                    (
                        e.name.clone(),
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

    /// The operator's file as an older or newer binary would meet it: keys this
    /// binary does not know (top level, in a known section, in a `[[target]]`
    /// element, in a subvolume entry) and comments of every kind (bd
    /// DAS-Backup-Manager-28il).
    pub(crate) const OPERATOR_FILE: &str = r#"# Operator's header: hand-tuned, keep.
future_top = "kept"

[general]
version = "0.4.0" # inline comment on a known key
install_prefix = "/usr/local"
db_path = "/var/lib/das-backup/backup-index.db"
future_general = 7

[init]
system = "systemd"

# Comment above [schedule].
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30

[recovery_os]
max_age_days = 60
egress_org = "AS64500"
future_recovery = "from a newer binary"

# Comment above the source.
[[source]]
label = "s"
volume = "/vol"
device = "/dev/sda"
future_source = true
[[source.subvolumes]]
name = "@"
future_entry = "kept with its entry"
[[source.subvolumes]]
name = "@gone"
retired = "2026-10-01"

# Comment above the target.
[[target]]
label = "t"
serials = ["SERIAL01"]
mount_uuid = "5eed0016-0000-4000-8000-000000000016"
mount = "/mnt/t"
role = "primary"
future_target = "kept with its target" # and its comment
[target.retention]
daily = 7

[email]
enabled = false

[gui]
enabled = false
"#;

    /// Every line of [`OPERATOR_FILE`] a save must keep whatever it changes.
    pub(crate) const OPERATOR_KEPT: &[&str] = &[
        "# Operator's header: hand-tuned, keep.",
        "future_top = \"kept\"",
        "# inline comment on a known key",
        "future_general = 7",
        "# Comment above [schedule].",
        "future_recovery = \"from a newer binary\"",
        "# Comment above the source.",
        "future_source = true",
        "future_entry = \"kept with its entry\"",
        "# Comment above the target.",
        "future_target = \"kept with its target\" # and its comment",
    ];

    /// Panics naming the first [`OPERATOR_KEPT`] line `text` lost.
    pub(crate) fn assert_operator_lines_kept(text: &str) {
        for kept in OPERATOR_KEPT {
            assert!(text.contains(kept), "lost {kept:?}:\n{text}");
        }
    }

    #[test]
    fn save_keeps_unknown_keys_and_comments_and_makes_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, OPERATOR_FILE).unwrap();
        let mut cfg = Config::load(&path).unwrap();
        // A changed scalar, an added entry, a deleted entry, a cleared Option.
        cfg.schedule.incremental = "04:15".into();
        cfg.general.version = "9.9.9".into();
        cfg.sources[0].subvolumes.retain(|e| e.name != "@gone");
        cfg.sources[0].subvolumes.push(SubvolConfig {
            name: "@new".into(),
            adopted: Some("2026-10-30".into()),
            ..Default::default()
        });
        cfg.targets[0].mount_uuid = None;
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_operator_lines_kept(&text);
        assert!(text.contains("egress_org = \"AS64500\""), "{text}");
        assert!(text.contains("incremental = \"04:15\""), "{text}");
        // A changed value keeps the comment on its line.
        assert!(
            text.contains("version = \"9.9.9\" # inline comment on a known key"),
            "{text}"
        );
        assert!(!text.contains("@gone"), "deleted entry survived:\n{text}");
        assert!(
            !text.contains("mount_uuid"),
            "cleared Option survived:\n{text}"
        );
        assert!(!text.contains(FRESH_HEADER), "{text}");
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(
            back.to_toml().unwrap(),
            cfg.with_pinned_snapshot_names().to_toml().unwrap()
        );
        // The unknown key of `@` stayed with `@`, not with a neighbour.
        let entry_at = text.find("name = \"@\"").unwrap();
        let future = text.find("future_entry").unwrap();
        let new_at = text.find("name = \"@new\"").unwrap();
        assert!(entry_at < future && future < new_at, "{text}");
        // A section the operator omitted whose defaults decide nothing about
        // what is backed up, where or for how long stays omitted; [boot] does
        // decide, so its current values are written out.
        assert!(!text.contains("[das]"), "{text}");
        assert!(text.contains("[boot]\nenabled = true\n"), "{text}");
    }

    /// Save `edit` of `file` through a real file; the text written.
    fn saved_after(file: &str, edit: impl FnOnce(&mut Config)) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, file).unwrap();
        let mut cfg = Config::load(&path).unwrap();
        edit(&mut cfg);
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            Config::from_toml(&text).unwrap().to_toml().unwrap(),
            cfg.with_pinned_snapshot_names().to_toml().unwrap(),
            "the saved file must read back as the config saved:\n{text}"
        );
        text
    }

    /// `file` as a save by this version leaves it with nothing changed: the
    /// backup-scoping keys it left out written in (`splice::PINNED`).
    fn pinned(file: &str) -> String {
        saved_after(file, |_| {})
    }

    #[test]
    fn save_leaves_an_unchanged_value_in_the_operators_spelling() {
        let file = minimal_toml(
            "[subvolumes]\n\
             exclude = [\n    \"@cache\",  # scratch\n    \"@old\",\n]\n\
             [[source]]\nlabel = \"s\"\nvolume = '/vol'\ndevice = \"/dev/sda\"\n\
             subvolumes = [\"@\", {name = \"@home\", future_inline = 1}]\n",
        );
        // An unrelated change; everything above is untouched.
        let file = pinned(&file);
        assert_eq!(pinned(&file), file, "pinning is done once");
        let text = saved_after(&file, |c| c.schedule.full = "Sat 05:00".into());
        assert!(text.contains("volume = '/vol'"), "{text}");
        assert!(text.contains("\"@cache\",  # scratch"), "{text}");
        // Pinned once: each entry names its snapshot series; the inline table
        // kept its spelling and unknown key.
        assert!(
            text.contains(
                "subvolumes = [{ name = \"@\", snapshot_name = \"root\" }, \
                 {name = \"@home\", future_inline = 1, snapshot_name = \"home\"}]"
            ),
            "{text}"
        );
        assert_eq!(text, file.replace("Sun 04:00", "Sat 05:00"));
    }

    #[test]
    fn save_edits_inline_subvolume_entries_in_place() {
        let file = minimal_toml(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             subvolumes = [\"@\", {name = \"@home\", future_inline = 1}, \"@srv\"]\n",
        );
        let text = saved_after(&file, |c| {
            let subs = &mut c.sources[0].subvolumes;
            subs[0].after_index = true;
            subs[1].retired = Some("2026-10-30".into());
            subs.push(SubvolConfig {
                name: "@new".into(),
                ..Default::default()
            });
        });
        let line = text.lines().find(|l| l.starts_with("subvolumes")).unwrap();
        // The changed bare string became a table; its unchanged neighbour did
        // not; the inline table kept its unknown key.
        assert!(
            line.contains("{ name = \"@\", snapshot_name = \"root\", after_index = true }"),
            "{line}"
        );
        assert!(line.contains("future_inline = 1"), "{line}");
        assert!(line.contains("retired = \"2026-10-30\""), "{line}");
        assert!(
            line.contains(", { name = \"@srv\", snapshot_name = \"srv\" },"),
            "{line}"
        );
        assert!(line.contains("name = \"@new\""), "{line}");
    }

    #[test]
    fn save_keeps_each_elements_unknown_keys_when_elements_move() {
        let two = "[[target]]\nlabel = \"a\"\nserials = [\"SERIAL01\"]\nmount = \"/mnt/a\"\n\
                   role = \"primary\"\nfuture = \"of a\"\n[target.retention]\ndaily = 7\n\
                   \n# Comment above b.\n\
                   [[target]]\nlabel = \"b\"\nserials = [\"SERIAL02\"]\nmount = \"/mnt/b\"\n\
                   role = \"mirror\"\nfuture = \"of b\"\n[target.retention]\ndaily = 3\n";
        let text = saved_after(&minimal_toml(two), |c| c.targets.swap(0, 1));
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(back.targets[0].label, "b", "{text}");
        let (b, a) = (
            text.find("label = \"b\"").unwrap(),
            text.find("label = \"a\"").unwrap(),
        );
        let (of_b, of_a) = (text.find("of b").unwrap(), text.find("of a").unwrap());
        let comment = text.find("# Comment above b.").unwrap();
        assert!(comment < b && b < of_b && of_b < a && a < of_a, "{text}");
        assert!(text.contains("daily = 3"), "{text}");
    }

    #[test]
    fn save_leaves_the_files_layout_alone_when_nothing_moved() {
        // Sections out of document order (a sub-table after a later section):
        // only a reorder may rewrite the layout.
        let file = minimal_toml(
            "[future]\na = 1\n[restore]\nallowed_roots = [\"/home\"]\n[future.sub]\nb = 2\n",
        );
        let file = pinned(&file);
        assert!(file.contains("[future.sub]\nb = 2\n"), "{file}");
        let text = saved_after(&file, |c| c.restore.allowed_roots = vec!["/tmp".into()]);
        assert_eq!(text, file.replace("[\"/home\"]", "[\"/tmp\"]"));
    }

    #[test]
    fn save_writes_added_and_emptied_arrays_of_tables() {
        let file = minimal_toml(&format!(
            "{}{ONE_TARGET}",
            source_with("s", "/vol", &[("@", None), ("@home", Some("home"))])
        ));
        let text = saved_after(&file, |c| {
            c.sources[0].subvolumes.clear();
            let mut added = c.sources[0].clone();
            added.label = "added".into();
            added.subvolumes = vec![SubvolConfig {
                name: "@x".into(),
                ..Default::default()
            }];
            c.sources.push(added);
            let mut t = c.targets[0].clone();
            t.label = "t2".into();
            t.mount = "/mnt/t2".into();
            c.targets.push(t);
        });
        assert!(text.contains("subvolumes = []"), "{text}");
        assert!(
            text.contains("[[source.subvolumes]]\nname = \"@x\""),
            "{text}"
        );
        // A new element's tables are written as sections, as a fresh file's are.
        assert_eq!(text.matches("[target.retention]\n").count(), 2, "{text}");
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(back.sources.len(), 2);
        assert_eq!(back.targets[1].label, "t2");
    }

    #[test]
    fn save_removes_a_deleted_element_with_everything_in_it() {
        let text = saved_after(OPERATOR_FILE, |c| c.targets.clear());
        assert!(!text.contains("future_target"), "{text}");
        assert!(!text.contains("[[target]]"), "{text}");
        assert!(text.contains("future_source = true"), "{text}");
    }

    /// A legacy `serial` (read, never written) beside a changed `serials` is
    /// removed: kept, it would be stale, and with `serials` cleared it would
    /// override the change on the next load.
    #[test]
    fn save_removes_a_legacy_serial_when_the_serials_it_fed_change() {
        let file = minimal_toml(ONE_TARGET);
        assert!(file.contains("serial = \"X\""));
        let text = saved_after(&file, |c| {
            c.targets[0].serials = vec!["SERIAL01".into(), "SERIAL02".into()];
        });
        assert!(
            !text.contains("serial = "),
            "stale legacy serial kept:\n{text}"
        );
        assert!(
            text.contains("serials = [\"SERIAL01\", \"SERIAL02\"]"),
            "{text}"
        );
        let text = saved_after(&file, |c| {
            c.targets[0].serials.clear();
            c.targets[0].serial.clear();
        });
        assert!(!text.contains("serial = "), "{text}");
        assert!(text.contains("serials = []"), "{text}");
        // Unchanged serials: the legacy key is the operator's spelling, kept.
        let text = saved_after(&file, |c| c.schedule.full = "Sat 05:00".into());
        assert!(text.contains("serial = \"X\""), "{text}");
    }

    /// The read-back refusal names what would differ.
    #[test]
    fn differing_paths_names_every_path_that_differs() {
        let a: toml::Table =
            toml::from_str("x = 1\nsame = 2\n[s]\nk = \"a\"\n[[t]]\nv = 1\n[[t]]\nv = 2\n")
                .unwrap();
        let b: toml::Table = toml::from_str(
            "x = 9\nsame = 2\nnew = 1\n[s]\nk = \"b\"\n[[t]]\nv = 1\n[[t]]\nv = 3\n",
        )
        .unwrap();
        assert_eq!(
            splice::differing_paths(&a, &b),
            ["x", "s.k", "t[1].v", "new"]
        );
        assert!(splice::differing_paths(&a, &a).is_empty());
        let short: toml::Table = toml::from_str("[[t]]\nv = 1\n").unwrap();
        let long: toml::Table = toml::from_str("[[t]]\nv = 1\n[[t]]\nv = 2\n").unwrap();
        assert_eq!(splice::differing_paths(&short, &long), ["t[1]"]);
        assert_eq!(splice::differing_paths(&long, &short), ["t[1]"]);
        // A plain array is one value.
        let one: toml::Table = toml::from_str("l = [1]\n").unwrap();
        let two: toml::Table = toml::from_str("l = [1, 2]\n").unwrap();
        assert_eq!(splice::differing_paths(&one, &two), ["l"]);
    }

    /// Comments inside a plain array stay with the elements still present
    /// (matched by value) when the array changes.
    #[test]
    fn save_keeps_the_comments_inside_a_changed_plain_array() {
        let file = pinned(&minimal_toml(
            "[subvolumes]\nexclude = [\n    \"@cache\", # scratch\n    \"@old\", # legacy\n] # end\n",
        ));
        let text = saved_after(&file, |c| c.subvolumes.exclude.push("@new".into()));
        for kept in ["# scratch", "# legacy", "] # end"] {
            assert!(text.contains(kept), "lost {kept:?}:\n{text}");
        }
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(back.subvolumes.exclude, ["@cache", "@old", "@new"]);
        let text = saved_after(&file, |c| c.subvolumes.exclude.retain(|e| e != "@cache"));
        assert!(text.contains("# legacy"), "{text}");
        assert!(!text.contains("@cache"), "{text}");
    }

    /// Two entries of one name: the one removed takes its own unknown keys
    /// with it — elements are matched on their known content first.
    #[test]
    fn save_removes_the_right_one_of_two_same_named_entries() {
        let file = minimal_toml(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             [[source.subvolumes]]\nname = \"@\"\nsnapshot_name = \"first\"\nfuture = \"of first\"\n\
             [[source.subvolumes]]\nname = \"@\"\nsnapshot_name = \"second\"\nfuture = \"of second\"\n",
        );
        let text = saved_after(&file, |c| {
            c.sources[0].subvolumes.remove(0);
        });
        assert!(!text.contains("of first"), "{text}");
        assert!(text.contains("future = \"of second\""), "{text}");
        // The kept one edited as well: still its own unknown key.
        let text = saved_after(&file, |c| {
            c.sources[0].subvolumes.remove(0);
            c.sources[0].subvolumes[0].after_index = true;
        });
        assert!(!text.contains("of first"), "{text}");
        assert!(text.contains("future = \"of second\""), "{text}");
        // Both kept, the first edited to look more like the second: each
        // continues itself, and no old entry is taken twice.
        let text = saved_after(&file, |c| {
            let first = &mut c.sources[0].subvolumes[0];
            first.snapshot_name = Some("second".into());
            first.after_index = true;
        });
        assert_eq!(text.matches("of first").count(), 1, "{text}");
        assert_eq!(text.matches("of second").count(), 1, "{text}");
        assert!(text.find("of first") < text.find("of second"), "{text}");
    }

    /// Unknown keys follow the element's identity, not whichever old element
    /// shares the most values with it.
    #[test]
    fn save_matches_elements_by_name_before_likeness() {
        let file = minimal_toml(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             [[source.subvolumes]]\nname = \"@a\"\nsnapshot_name = \"x\"\nfuture = \"of a\"\n\
             [[source.subvolumes]]\nname = \"@b\"\nafter_index = true\nsnapshot_name = \"y\"\n\
             future = \"of b\"\n",
        );
        // @a now looks like the old @b in everything but its name; @b changed
        // too, so neither is matched unchanged.
        let text = saved_after(&file, |c| {
            let a = &mut c.sources[0].subvolumes[0];
            a.snapshot_name = Some("y".into());
            a.after_index = true;
            c.sources[0].subvolumes[1].snapshot_name = Some("z".into());
        });
        let (a, of_a) = (
            text.find("name = \"@a\"").unwrap(),
            text.find("of a").unwrap(),
        );
        let (b, of_b) = (
            text.find("name = \"@b\"").unwrap(),
            text.find("of b").unwrap(),
        );
        assert!(a < of_a && of_a < b && b < of_b, "{text}");
    }

    /// The keys that decide WHAT is backed up, WHERE it goes and for HOW LONG
    /// are written out with their current values when the file leaves them
    /// out, so a later version's different compiled default cannot change a
    /// backup silently. Every other default stays left out.
    #[test]
    fn save_writes_out_backup_scoping_defaults_and_leaves_the_rest_out() {
        let file = minimal_toml(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             [[source.subvolumes]]\nname = \"@\"\n\
             [[source]]\nlabel = \"s2\"\nvolume = \"/vol2\"\ndevice = \"/dev/sdb\"\n\
             snapshot_dir = \"snaps\"\ntarget_labels = [\"t\"]\n\
             [[source.subvolumes]]\nname = \"@x\"\n\
             [[target]]\nlabel = \"t\"\nmount = \"/mnt/t\"\nrole = \"primary\"\n\
             [target.retention]\ndaily = 7\n",
        );
        let text = saved_after(&file, |c| c.schedule.full = "Sat 05:00".into());
        let back = Config::from_toml(&text).unwrap();
        for pinned in [
            "btrbk_conf = \"/etc/btrbk/btrbk.conf\"",
            "[boot]\nenabled = true\nsubvolumes = [\"@\", \"@home\"]\narchive_retention_days = 60\n",
            "[subvolumes]\nexclude = []\n",
            "[doctor]\nexclude = []\n",
            "label = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             snapshot_dir = \".btrbk-snapshots\"\ntarget_subdirs = []\ntarget_labels = []\n",
            "[target.retention]\ndaily = 7\nweekly = 0\nmonthly = 0\nyearly = 0\n",
        ] {
            assert!(text.contains(pinned), "not written out: {pinned:?}\n{text}");
        }
        // A value the operator wrote is not touched.
        assert!(
            text.contains("snapshot_dir = \"snaps\"\ntarget_labels = [\"t\"]\n"),
            "{text}"
        );
        assert_eq!(back.sources[1].snapshot_dir, "snaps");
        for absent in [
            "[das]",
            "[scrub]",
            "[recovery_os]",
            "log_file",
            "growth_log",
            "last_report",
            "recovery_drive",
            "manual_only",
            "display_name",
            "serials",
            "smtp_host",
        ] {
            assert!(!text.contains(absent), "written out: {absent:?}\n{text}");
        }
    }

    #[test]
    fn save_text_writes_the_submitted_text_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, minimal_toml("")).unwrap();
        // Already holding every backup-scoping key: written as it is.
        let submitted = pinned(&format!("# typed in the editor\n{OPERATOR_FILE}"))
            .replace("future_top = \"kept\"", "future_typed = \"by hand\" # note");
        Config::save_text(&submitted, &path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), submitted);
        // Missing the backup-scoping keys: they are written out, nothing else
        // changes.
        Config::save_text(OPERATOR_FILE, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Operator's header"), "{text}");
        assert_operator_lines_kept(&text);
        assert!(text.contains("[boot]\nenabled = true\n"), "{text}");
        // Not a config: refused, the file as it was.
        for bad in ["not [ toml", "[general]\nversion = 1\n"] {
            let err = Config::save_text(bad, &path).unwrap_err().to_string();
            assert!(
                err.starts_with(&format!("cannot write {}: ", path.display())),
                "{err}"
            );
            assert!(err.contains("the submitted text"), "{err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
    }

    /// The line of `text` holding `needle` (which must be on exactly one).
    fn line_with<'t>(text: &'t str, needle: &str) -> &'t str {
        let lines: Vec<&str> = text.lines().filter(|l| l.contains(needle)).collect();
        assert_eq!(
            lines.len(),
            1,
            "{needle:?} on {} lines:\n{text}",
            lines.len()
        );
        lines[0]
    }

    /// A comment after an array element's comma belongs to that element:
    /// toml_edit files it as the NEXT element's prefix (the last one's in the
    /// array's trailing), so a naive rebuild shifted comments by one.
    #[test]
    fn a_plain_array_keeps_each_comment_beside_its_own_value() {
        let file = pinned(&minimal_toml(
            "[subvolumes]\nexclude = [\n    \"@cache\", # about cache\n    \"@old\", # about old\n    \
             \"@srv\", # about srv\n]\n",
        ));
        let placed = |text: &str, names: &[&str], gone: &[&str]| {
            for n in names {
                let line = line_with(text, &format!("\"@{n}\""));
                assert!(
                    line.contains(&format!("# about {n}")),
                    "{n}: {line:?}\n{text}"
                );
            }
            for g in gone {
                assert!(
                    !text.contains(&format!("about {g}")),
                    "{g} survived:\n{text}"
                );
            }
        };
        let ex = |c: &mut Config, v: &[&str]| {
            c.subvolumes.exclude = v.iter().map(|s| s.to_string()).collect()
        };
        let t = saved_after(&file, |c| ex(c, &["@old", "@srv"]));
        placed(&t, &["old", "srv"], &["cache"]);
        let t = saved_after(&file, |c| ex(c, &["@cache", "@srv"]));
        placed(&t, &["cache", "srv"], &["old"]);
        let t = saved_after(&file, |c| ex(c, &["@cache", "@old"]));
        placed(&t, &["cache", "old"], &["srv"]);
        let t = saved_after(&file, |c| ex(c, &["@cache", "@old", "@srv", "@new"]));
        placed(&t, &["cache", "old", "srv"], &[]);
        assert!(!line_with(&t, "\"@new\"").contains('#'), "{t}");
        let t = saved_after(&file, |c| ex(c, &["@srv", "@cache", "@old"]));
        placed(&t, &["srv", "cache", "old"], &[]);
        // A single-line array stays on one line.
        let one = pinned(&minimal_toml("[subvolumes]\nexclude = [\"@a\", \"@b\"]\n"));
        let t = saved_after(&one, |c| ex(c, &["@b", "@c"]));
        assert!(t.contains("exclude = [\"@b\", \"@c\"]"), "{t}");
    }

    /// The same for subvolume entries in an inline array.
    #[test]
    fn an_inline_entry_array_keeps_each_comment_beside_its_own_entry() {
        let file = pinned(&minimal_toml(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             subvolumes = [\n    \"@\", # root\n    \"@home\", # homes\n    \"@srv\", # svc\n]\n",
        ));
        let t = saved_after(&file, |c| {
            c.sources[0].subvolumes.retain(|e| e.name != "@home")
        });
        assert!(line_with(&t, "name = \"@\"").contains("# root"), "{t}");
        assert!(line_with(&t, "name = \"@srv\"").contains("# svc"), "{t}");
        assert!(!t.contains("homes"), "{t}");
        let t = saved_after(&file, |c| c.sources[0].subvolumes.swap(0, 2));
        assert!(line_with(&t, "name = \"@\"").contains("# root"), "{t}");
        assert!(line_with(&t, "name = \"@srv\"").contains("# svc"), "{t}");
        assert!(line_with(&t, "name = \"@home\"").contains("# homes"), "{t}");
    }

    /// Arrays of tables: an entry kept in a new order moves with the comment
    /// block touching its last line and the one above its header (after a
    /// blank line). An entry removed takes only its own lines — header to
    /// last line — and the comment lines around it stay where they are (bd
    /// DAS-Backup-Manager-5237.4; 0.7.23.2 dropped them with the entry).
    #[test]
    fn an_array_of_tables_keeps_each_comment_block_with_its_own_entry() {
        let file = minimal_toml(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             [[source.subvolumes]]\nname = \"@a\"\nsnapshot_name = \"a\"\n# after a\n\
             \n# above b\n[[source.subvolumes]]\nname = \"@b\"\nsnapshot_name = \"b\"\n# after b\n\
             \n# above c\n[[source.subvolumes]]\nname = \"@c\"\nsnapshot_name = \"c\"\n# after c\n",
        );
        let file = pinned(&file);
        let kept = |t: &str, n: &str| {
            assert!(
                t.contains(&format!("snapshot_name = \"{n}\"\n# after {n}\n")),
                "{n}:\n{t}"
            );
            assert!(
                t.contains(&format!(
                    "# above {n}\n[[source.subvolumes]]\nname = \"@{n}\""
                )),
                "{n}:\n{t}"
            );
        };
        let gone = |t: &str, n: &str| {
            assert!(!t.contains(&format!("name = \"@{n}\"")), "{n}:\n{t}");
        };
        let without =
            |n: &'static str| move |c: &mut Config| c.sources[0].subvolumes.retain(|e| e.name != n);
        let t = saved_after(&file, without("@b"));
        kept(&t, "c");
        gone(&t, "b");
        assert!(
            t.contains(
                "snapshot_name = \"a\"\n# after a\n\n# above b\n# after b\n\n# above c\n\
                 [[source.subvolumes]]\nname = \"@c\""
            ),
            "{t}"
        );
        let t = saved_after(&file, without("@c"));
        kept(&t, "b");
        gone(&t, "c");
        assert!(
            t.contains("snapshot_name = \"b\"\n# after b\n\n# above c\n# after c\n"),
            "{t}"
        );
        let t = saved_after(&file, without("@a"));
        kept(&t, "b");
        kept(&t, "c");
        gone(&t, "a");
        assert!(
            t.contains(
                "target_labels = []\n# after a\n\n# above b\n[[source.subvolumes]]\nname = \"@b\""
            ),
            "{t}"
        );
        let t = saved_after(&file, |c| c.sources[0].subvolumes.swap(0, 2));
        kept(&t, "b");
        assert!(t.find("name = \"@c\"") < t.find("# after c"), "{t}");
        assert!(t.find("# after c") < t.find("name = \"@b\""), "{t}");
        // A key added at the end of an entry goes above its trailing block.
        let t = saved_after(&file, |c| {
            c.sources[0].subvolumes[0].adopted = Some("2026-10-31".into())
        });
        assert!(t.contains("adopted = \"2026-10-31\"\n# after a\n"), "{t}");
        // A key removed from the end of an entry hands its block up.
        let retired = file.replace(
            "snapshot_name = \"a\"\n",
            "snapshot_name = \"a\"\nretired = \"2026-10-01\"\n",
        );
        let t = saved_after(&retired, |c| c.sources[0].subvolumes[0].retired = None);
        assert!(t.contains("snapshot_name = \"a\"\n# after a\n"), "{t}");
    }

    /// A key written out into an inline table takes the spacing before `}`
    /// with it.
    #[test]
    fn a_key_written_into_an_inline_table_keeps_its_spacing() {
        let file = format!(
            "target = [{{ label = \"t\", mount = \"/mnt/t\", role = \"primary\", \
             retention = {{ daily = 7 }} }}]\n{}",
            minimal_toml("")
        );
        let t = saved_after(&file, |c| c.schedule.full = "Sat 05:00".into());
        assert!(
            t.contains("retention = { daily = 7, weekly = 0, monthly = 0, yearly = 0 } }]"),
            "{t}"
        );
    }

    /// Of two same-named entries that share as many values with the one kept,
    /// the earlier continues it.
    #[test]
    fn of_two_equally_alike_same_named_entries_the_earlier_is_kept() {
        let file = minimal_toml(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             [[source.subvolumes]]\nname = \"@\"\nsnapshot_name = \"a\"\nfuture = \"of first\"\n\
             [[source.subvolumes]]\nname = \"@\"\nsnapshot_name = \"b\"\nfuture = \"of second\"\n",
        );
        let t = saved_after(&file, |c| {
            c.sources[0].subvolumes.truncate(1);
            c.sources[0].subvolumes[0].snapshot_name = Some("c".into());
        });
        assert!(t.contains("of first") && !t.contains("of second"), "{t}");
    }

    /// The read-back check is what stops a faulty edit: a merge fault is
    /// refused, the paths named, nothing written.
    #[test]
    fn a_faulty_edit_is_refused_naming_what_would_differ() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let file = pinned(&minimal_toml(ONE_TARGET));
        std::fs::write(&path, &file).unwrap();
        let mut cfg = Config::load(&path).unwrap();
        cfg.schedule.full = "Sat 05:00".into();
        let fault = |text: &mut String| {
            *text = text
                .replace("full = \"Sat 05:00\"", "full = \"Mon 01:00\"")
                .replace("daily = 7", "daily = 1");
        };
        let err = cfg.save_with(&path, &fault).unwrap_err().to_string();
        assert!(err.contains("would not read back"), "{err}");
        assert!(
            err.contains("schedule.full, target[0].retention.daily would differ"),
            "{err}"
        );
        assert_eq!(err.matches(&path.display().to_string()).count(), 1, "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file);
        // With no fault the same save goes through.
        cfg.save_with(&path, &|_| {}).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("Sat 05:00")
        );
    }

    /// The restore allow-list is a security boundary: pinned like the
    /// backup-scoping keys.
    #[test]
    fn save_writes_out_the_restore_allow_list() {
        let t = saved_after(&minimal_toml(""), |c| c.schedule.full = "Sat 05:00".into());
        assert!(
            t.contains("[restore]\nallowed_roots = [\"/home\", \"/tmp\"]\n"),
            "{t}"
        );
    }

    /// Every entry's snapshot name is written out as btrbk uses it today, so a
    /// later change to the derivation can never rename a series (and restart
    /// its send chain); btrbk.conf renders the same before and after.
    #[test]
    fn save_writes_out_the_snapshot_name_btrbk_uses_today() {
        let file = minimal_toml(&format!(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             subvolumes = [\"@\", \"@srv/web\", \"@home\", \"home\"]\n\
             [[source]]\nlabel = \"top\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             [[source.subvolumes]]\nname = \".\"\nsnapshot_name = \"vol-toplevel\"\n\
             [[source.subvolumes]]\nname = \"@gone\"\nretired = \"2026-10-01\"\n{ONE_TARGET}"
        ));
        let before = Config::from_toml(&file).unwrap();
        let text = saved_after(&file, |c| c.schedule.full = "Sat 05:00".into());
        let after = Config::from_toml(&text).unwrap();
        let names = |c: &Config| -> Vec<Option<String>> {
            c.sources
                .iter()
                .flat_map(|s| s.subvolumes.iter().map(|e| e.snapshot_name.clone()))
                .collect()
        };
        assert_eq!(
            names(&after),
            [
                Some("root".into()),
                Some("srv-web".into()),
                Some("root-home".into()),
                Some("root-home".into()),
                Some("vol-toplevel".into()),
                Some("gone".into()),
            ]
        );
        let mut same = after.clone();
        same.schedule.full = before.schedule.full.clone();
        assert_eq!(
            crate::btrbk_conf::render_btrbk_conf(&same).unwrap(),
            crate::btrbk_conf::render_btrbk_conf(&before).unwrap(),
            "pinning must not change what btrbk is told"
        );
        // The top level keeps its explicit name, untouched.
        assert!(
            text.contains(
                "[[source.subvolumes]]\nname = \".\"\nsnapshot_name = \"vol-toplevel\"\n"
            ),
            "{text}"
        );
        // Bare entries become tables holding only the name and what was added.
        assert!(
            text.contains("{ name = \"@\", snapshot_name = \"root\" }"),
            "{text}"
        );
    }

    /// A key a known table does not have is warned about, with the nearest
    /// key it does have; a clean config warns nothing.
    #[test]
    fn unknown_keys_in_known_tables_are_warned_with_a_hint() {
        // A top-level key of its own (a newer version's section) is expected,
        // and not warned about.
        let text = format!(
            "future_section = 1\n{}",
            minimal_toml(
                "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
                 target_lables = [\"t\"]\n[[source.subvolumes]]\nname = \"@\"\nsnapshot_nme = \"x\"\n\
                 [[target]]\nlabel = \"t\"\nserial = \"X\"\nmount = \"/mnt/t\"\nrole = \"primary\"\n\
                 [target.retention]\ndaily = 7\nmonthy = 1\n"
            )
        )
        .replace("[gui]\nenabled = false\n", "[gui]\nenabled = false\ntheme = \"dark\"\n");
        let warnings = unknown_key_warnings(&text).unwrap();
        assert_eq!(
            warnings,
            [
                "gui.theme is not a key this version reads",
                "source[0].target_lables is not a key this version reads (did you mean target_labels?)",
                "source[0].subvolumes[0].snapshot_nme is not a key this version reads (did you mean snapshot_name?)",
                "target[0].retention.monthy is not a key this version reads (did you mean monthly?)",
            ],
            "{text}"
        );
        // Legacy `serial`, a bare-string entry and `after_index = false` are
        // known; so is everything a fresh file holds.
        let clean = minimal_toml(&format!(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             subvolumes = [\"@\", {{ name = \"@x\", after_index = false }}]\n{ONE_TARGET}"
        ));
        assert_eq!(unknown_key_warnings(&clean).unwrap(), Vec::<String>::new());
        let fresh = Config::from_toml(&clean).unwrap().to_toml().unwrap();
        assert_eq!(unknown_key_warnings(&fresh).unwrap(), Vec::<String>::new());
        assert!(unknown_key_warnings("not [ toml").is_err());
    }

    /// bd DAS-Backup-Manager-9ry (operator 2026-11-09): `manual_only` is
    /// removed. A file still carrying it loads — the subvolume is backed up
    /// like any other — and the warning says the key is gone, not a typo.
    #[test]
    fn an_old_config_naming_manual_only_still_loads_and_is_told_the_key_is_gone() {
        let text = minimal_toml(&format!(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             [[source.subvolumes]]\nname = \"@a\"\nmanual_only = true\n\
             [[source.subvolumes]]\nname = \"@b\"\nmanual_only = false\n{ONE_TARGET}"
        ));
        let cfg = Config::from_toml(&text).unwrap();
        let names: Vec<&str> = cfg.sources[0]
            .subvolumes
            .iter()
            .map(|sv| sv.name.as_str())
            .collect();
        assert_eq!(names, ["@a", "@b"]);
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let gone = "is no longer read: manual_only was removed, and every backup run \
                    backs this subvolume up \u{2014} delete the key";
        assert_eq!(
            unknown_key_warnings(&text).unwrap(),
            [
                format!("source[0].subvolumes[0].manual_only {gone}"),
                format!("source[0].subvolumes[1].manual_only {gone}"),
            ]
        );
    }

    /// bd DAS-Backup-Manager-s5jv.4: a removed key is told as removed only
    /// where it used to be read — a subvolume entry — not wherever the name
    /// turns up.
    #[test]
    fn manual_only_outside_a_subvolume_entry_is_an_unknown_key_not_a_removed_one() {
        let text = minimal_toml(&format!(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             manual_only = true\n[[source.subvolumes]]\nname = \"@a\"\nmanual_only = true\n{ONE_TARGET}"
        ))
        .replace("[general]\n", "[general]\nmanual_only = 1\n");
        let warnings = unknown_key_warnings(&text).unwrap();
        assert!(
            warnings.contains(&"general.manual_only is not a key this version reads".to_string()),
            "{warnings:?}"
        );
        assert!(
            warnings.contains(&"source[0].manual_only is not a key this version reads".to_string()),
            "{warnings:?}"
        );
        // Positive control: the entry's own key is still the removed one.
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("source[0].subvolumes[0].manual_only is no longer read")),
            "{warnings:?}"
        );
    }

    /// bd DAS-Backup-Manager-9ry: a save takes the removed key out of every
    /// subvolume entry, changed or not — table and inline — so the warning
    /// stops; the entry's other unknown keys stay.
    #[test]
    fn a_save_drops_manual_only_from_every_subvolume_entry() {
        let file = minimal_toml(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             subvolumes = [{ name = \"@\", manual_only = false, future_inline = 1 }]\n\
             [[source]]\nlabel = \"t\"\nvolume = \"/vol2\"\ndevice = \"/dev/sdb\"\n\
             [[source.subvolumes]]\nname = \"@a\"\nmanual_only = true\nfuture = 2\n",
        );
        // An unrelated edit: neither entry changes.
        let text = saved_after(&file, |c| c.schedule.randomized_delay_min = 31);
        assert!(!text.contains("manual_only"), "{text}");
        assert!(text.contains("future_inline = 1"), "{text}");
        assert!(text.contains("future = 2"), "{text}");
        assert_eq!(unknown_key_warnings(&text).unwrap().len(), 2, "{text}");
    }

    /// bd DAS-Backup-Manager-s5jv.4: the GUI's whole-text save goes through
    /// `save_text`, which must take the removed key out as a structured save
    /// does, with the operator's other text kept.
    #[test]
    fn a_text_save_drops_manual_only_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let typed = pinned(&minimal_toml(
            "[[source]]\nlabel = \"s\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             subvolumes = [{ name = \"@\", manual_only = false, future_inline = 1 }]\n\
             [[source]]\nlabel = \"t\"\nvolume = \"/vol2\"\ndevice = \"/dev/sdb\"\n\
             [[source.subvolumes]]\nname = \"@a\"\nmanual_only = true\nfuture = 2\n",
        ));
        Config::save_text(&typed, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("manual_only"), "{text}");
        assert!(text.contains("future_inline = 1"), "{text}");
        assert!(text.contains("future = 2"), "{text}");
    }

    /// The Backup panel's contract: gui/tests/fixtures/panel-config.json IS
    /// what `panel_json` gives for each case's text (regenerate with
    /// PANEL_FIXTURE_WRITE=1). The cases are the ones a line-by-line reader of
    /// the raw file got wrong.
    #[test]
    fn the_gui_panel_fixture_is_this_output() {
        let base = minimal_toml("");
        let src = |label: &str| {
            format!(
                "[[source]]\nlabel = {label}\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\nsubvolumes = [\"@\"]\n"
            )
        };
        let tgt = |label: &str| {
            format!(
                "[[target]]\nlabel = {label}\nmount = \"/mnt/t\"\nrole = \"primary\"\n[target.retention]\ndaily = 7\n"
            )
        };
        let cases: Vec<(&str, String)> = vec![
            ("spaced boot header", format!("{base}[ boot ]\nenabled = false\n{}{}", src("\"s\""), tgt("\"t\""))),
            ("inline boot table", format!("boot = {{ enabled = false }}\n{base}{}{}", src("\"s\""), tgt("\"t\""))),
            ("spaced source header", format!("{base}[[ source ]]\nlabel = \"s\"\nvolume = \"/v\"\ndevice = \"/d\"\nsubvolumes = [\"@\"]\n{}", tgt("\"t\""))),
            (
                "header inside a multi-line string",
                format!("{base}{}{}", src("\"s\"").replace("device = \"/dev/sda\"\n", "device = \"/dev/sda\"\nnote = \"\"\"\n[[target]]\nlabel = \"ghost\"\n\"\"\"\n"), tgt("\"t\"")),
            ),
            ("escaped label", format!("{base}{}{}", src("\"a\\\"b\""), tgt("'lit'"))),
        ];
        let doc: Vec<serde_json::Value> = cases
            .iter()
            .map(|(name, text)| {
                let panel: serde_json::Value =
                    serde_json::from_str(&Config::from_toml(text).unwrap().panel_json()).unwrap();
                serde_json::json!({"case": name, "toml": text, "panel": panel})
            })
            .collect();
        let out = serde_json::to_string_pretty(&doc).unwrap() + "\n";
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../gui/tests/fixtures/panel-config.json"
        );
        if std::env::var_os("PANEL_FIXTURE_WRITE").is_some() {
            std::fs::write(path, &out).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(path).unwrap_or_default(),
            out,
            "the fixture is stale: PANEL_FIXTURE_WRITE=1 cargo test the_gui_panel_fixture"
        );
        let first: serde_json::Value =
            serde_json::from_str(&Config::from_toml(&cases[0].1).unwrap().panel_json()).unwrap();
        assert_eq!(
            first,
            serde_json::json!({"sources": ["s"], "targets": ["t"], "boot_enabled": false})
        );
        let ghost: serde_json::Value =
            serde_json::from_str(&Config::from_toml(&cases[3].1).unwrap().panel_json()).unwrap();
        assert_eq!(ghost["targets"], serde_json::json!(["t"]));
    }

    #[test]
    fn edit_distance_counts_single_character_edits() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("ab", "ba"), 2);
        assert_eq!(edit_distance("monthy", "monthly"), 1);
        assert_eq!(edit_distance("same", "same"), 0);
    }

    /// The layout of plain arrays, exactly: a comment line above an element
    /// stays above it, a new element takes its neighbours' spacing, and a
    /// literal string kept unchanged keeps its quotes.
    #[test]
    fn a_plain_array_keeps_its_layout_exactly() {
        let ex = |c: &mut Config, v: &[&str]| {
            c.subvolumes.exclude = v.iter().map(|s| s.to_string()).collect()
        };
        let multi = pinned(&minimal_toml(
            "[subvolumes]\nexclude = [\n    \"@a\",\n    # above b\n    \"@b\",\n]\n",
        ));
        let t = saved_after(&multi, |c| ex(c, &["@b"]));
        assert!(
            t.contains("exclude = [\n    # above b\n    \"@b\",\n]\n"),
            "{t}"
        );
        let one = pinned(&minimal_toml("[subvolumes]\nexclude = [\n    \"@a\",\n]\n"));
        let t = saved_after(&one, |c| ex(c, &["@a", "@b"]));
        assert!(
            t.contains("exclude = [\n    \"@a\",\n    \"@b\",\n]\n"),
            "{t}"
        );
        let single = pinned(&minimal_toml("[subvolumes]\nexclude = ['@a']\n"));
        let t = saved_after(&single, |c| ex(c, &["@a", "@b"]));
        assert!(t.contains("exclude = ['@a', \"@b\"]\n"), "{t}");
    }

    /// Comment blocks next to tables this version does not have, next to an
    /// empty table and at the top of the file stay exactly where they are on
    /// a save that changes nothing near them.
    #[test]
    fn comments_around_unknown_empty_and_first_tables_stay_put() {
        let file = format!(
            "# top of the file\n{}[futa]\nx = 1\n# after futa\n[futb.c]\ny = 2\n\n[futc]\n# about futc\n\n[futd]\nz = 3\n",
            minimal_toml("").trim_start()
        );
        let file = pinned(&file);
        // As the operator wrote them, not only as a first save left them.
        assert!(file.starts_with("# top of the file\n[general]\n"), "{file}");
        assert!(
            file.contains("x = 1\n# after futa\n[futb.c]\ny = 2\n"),
            "{file}"
        );
        assert!(
            file.contains("y = 2\n\n[futc]\n# about futc\n\n[futd]\nz = 3\n"),
            "{file}"
        );
        let t = saved_after(&file, |c| c.schedule.full = "Sat 05:00".into());
        assert_eq!(t, file.replace("Sun 04:00", "Sat 05:00"));
    }

    /// A top-level entry with no name of its own is not given one: it must
    /// name itself (validate refuses it until it does).
    #[test]
    fn a_top_level_entry_without_a_name_is_not_named_by_a_save() {
        let file = minimal_toml(
            "[[source]]\nlabel = \"top\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             [[source.subvolumes]]\nname = \".\"\n",
        );
        let t = saved_after(&file, |c| c.schedule.full = "Sat 05:00".into());
        assert!(t.contains("[[source.subvolumes]]\nname = \".\"\n"), "{t}");
        let back = Config::from_toml(&t).unwrap();
        assert_eq!(back.sources[0].subvolumes[0].snapshot_name, None);
    }

    // -----------------------------------------------------------------------
    // Byte-exact saves (bd DAS-Backup-Manager-5237.4): each case asserts the
    // saved file IS the input with only the planned splices — never a
    // semantic comparison. The cases are verify4's findings on the m0pr
    // scanner (`m0pr-verify4-result.json`, numbered as there) and bd 5237.1,
    // 5237.2, 5237.3 and m0pr.3.
    // -----------------------------------------------------------------------

    /// A head holding every backup-scoping key, so a save pins nothing in it.
    const SPLICE_HEAD: &str = "[general]\nversion = \"0.4.0\"\ninstall_prefix = \"/usr/local\"\n\
        db_path = \"/var/lib/das-backup/backup-index.db\"\nbtrbk_conf = \"/etc/btrbk/btrbk.conf\"\n\
        \n[init]\nsystem = \"systemd\"\n\
        \n[schedule]\nincremental = \"03:00\"\nfull = \"Sun 04:00\"\nrandomized_delay_min = 30\n\
        \n[boot]\nenabled = true\nsubvolumes = [\"@\", \"@home\"]\narchive_retention_days = 60\n\
        \n[subvolumes]\nexclude = []\n\
        \n[doctor]\nexclude = []\n\
        \n[restore]\nallowed_roots = [\"/home\", \"/tmp\"]\n\
        \n[email]\nenabled = false\n\
        \n[gui]\nenabled = false\n\n";

    /// A `[[source]]` holding every pinned key, `label` = `s`, then `rest`
    /// (its `subvolumes`, inline or as entries).
    fn src(label: &str, rest: &str) -> String {
        format!(
            "[[source]]\nlabel = \"{label}\"\nvolume = \"/vol\"\ndevice = \"/dev/sda\"\n\
             snapshot_dir = \".btrbk-snapshots\"\ntarget_subdirs = []\ntarget_labels = []\n{rest}"
        )
    }

    /// A `[[target]]` holding every pinned key.
    fn tgt(i: usize) -> String {
        format!(
            "[[target]]\nlabel = \"t{i}\"\nserials = [\"SERIAL0{i}\"]\nmount = \"/mnt/backup-{i}\"\n\
             role = \"primary\"\nretention = {{ daily = 1, weekly = 0, monthly = 0, yearly = 0 }}\n"
        )
    }

    /// `file` with each `(from, to)` replaced; each `from` must occur exactly
    /// once, so the expected text names the only bytes a save may change.
    fn spliced(file: &str, edits: &[(&str, &str)]) -> String {
        let mut out = file.to_string();
        for (from, to) in edits {
            assert_eq!(
                out.matches(from).count(),
                1,
                "{from:?} is not unique in:\n{out}"
            );
            out = out.replacen(from, to, 1);
        }
        out
    }

    /// Save `edit` of `file` through a real file: the bytes written and the
    /// save's warnings.
    fn exact(file: &str, edit: impl FnOnce(&mut Config)) -> (String, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, file).unwrap();
        let mut cfg = Config::load(&path).unwrap();
        edit(&mut cfg);
        let mut warnings = Vec::new();
        cfg.save_reporting(&path, &|_| {}, &mut |w| warnings.push(w))
            .unwrap();
        (std::fs::read_to_string(&path).unwrap(), warnings)
    }

    fn exact_text(file: &str, edit: impl FnOnce(&mut Config)) -> String {
        exact(file, edit).0
    }

    fn sub(name: &str, snap: &str) -> SubvolConfig {
        SubvolConfig {
            name: name.into(),
            snapshot_name: Some(snap.into()),
            ..Default::default()
        }
    }

    const SUB_A: &str = "subvolumes = [{ name = \"@a\", snapshot_name = \"a\" }]\n";

    /// #0: a key added to a multi-line inline table whose `}` shares the last
    /// value's line goes on a line of its own, at the lines' indentation.
    #[test]
    fn splice_a_key_added_to_a_multi_line_inline_table_takes_a_line_of_its_own() {
        let egress = |c: &mut Config| c.recovery_os.egress_org = Some("AS64500".into());
        let body = format!("{SPLICE_HEAD}{}{}", src("s", SUB_A), tgt(0));
        for (table, want) in [
            (
                "recovery_os = {\n  max_age_days = 30 }\n",
                "recovery_os = {\n  max_age_days = 30,\n  egress_org = \"AS64500\" }\n",
            ),
            (
                "recovery_os = {\n  max_age_days = 30, }\n",
                "recovery_os = {\n  max_age_days = 30,\n  egress_org = \"AS64500\", }\n",
            ),
            (
                "recovery_os = {\n  install_user = \"user\" } # after\n",
                "recovery_os = {\n  install_user = \"user\",\n  egress_org = \"AS64500\" } # after\n",
            ),
        ] {
            let file = format!("{table}{body}");
            assert_eq!(exact_text(&file, egress), spliced(&file, &[(table, want)]));
        }
        // A pinned key the same way.
        let file = format!("{SPLICE_HEAD}{}", src("s", SUB_A))
            + "[[target]]\nlabel = \"t0\"\nserials = [\"SERIAL01\"]\nmount = \"/mnt/backup-0\"\n\
               role = \"primary\"\nretention = {\n    daily = 7 }\n";
        assert_eq!(
            exact_text(&file, |_| {}),
            spliced(
                &file,
                &[(
                    "    daily = 7 }",
                    "    daily = 7,\n    weekly = 0,\n    monthly = 0,\n    yearly = 0 }"
                )]
            )
        );
    }

    /// #1: removing the last entry of an inline array leaves a `]` that stood
    /// on its own line there.
    #[test]
    fn splice_removing_the_last_inline_entry_keeps_a_closing_bracket_on_its_line() {
        let first = "{ name = \"@\", snapshot_name = \"root\" }";
        let b = "{ name = \"@b\", snapshot_name = \"b\" }";
        let drop_b = |c: &mut Config| c.sources[0].subvolumes.retain(|e| e.name != "@b");
        for (line, want, warned) in [
            (
                format!("subvolumes = [{first}, {b},\n]\n"),
                format!("subvolumes = [{first},\n]\n"),
                false,
            ),
            (
                format!("subvolumes = [{first}, {b}\n]\n"),
                format!("subvolumes = [{first}\n]\n"),
                false,
            ),
            (
                format!("subvolumes = [{first}, {b}, # b note\n]\n"),
                format!("subvolumes = [{first},\n]\n"),
                true,
            ),
        ] {
            let file = format!("{SPLICE_HEAD}{}{}", src("s", &line), tgt(0));
            let (text, warnings) = exact(&file, drop_b);
            assert_eq!(text, spliced(&file, &[(line.as_str(), want.as_str())]));
            assert_eq!(warnings.len(), usize::from(warned), "{warnings:?}");
        }
    }

    /// #2: removing the first entry, on the `[` line, leaves the next line
    /// where it was.
    #[test]
    fn splice_removing_the_first_inline_entry_leaves_the_next_line_alone() {
        let line = "subvolumes = [{ name = \"@\", snapshot_name = \"root\" },\n    \
                    { name = \"@b\", snapshot_name = \"b\" },\n]\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", line), tgt(0));
        let text = exact_text(&file, |c| {
            c.sources[0].subvolumes.remove(0);
        });
        assert_eq!(
            text,
            spliced(
                &file,
                &[("[{ name = \"@\", snapshot_name = \"root\" },\n", "[\n")]
            )
        );
    }

    /// #3: an entry added to a multi-line array whose `]` shares the last
    /// entry's line goes on its own line at the entries' indentation; removing
    /// it again gives back the input.
    #[test]
    fn splice_an_entry_added_beside_a_shared_closing_bracket() {
        let line = "subvolumes = [ { name = \"@\", snapshot_name = \"root\" },\n               \
                    { name = \"@b\", snapshot_name = \"b\" } ]\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", line), tgt(0));
        let added = exact_text(&file, |c| c.sources[0].subvolumes.push(sub("@new", "new")));
        assert_eq!(
            added,
            spliced(
                &file,
                &[(
                    "snapshot_name = \"b\" } ]",
                    "snapshot_name = \"b\" },\n               \
                     { name = \"@new\", snapshot_name = \"new\" } ]"
                )]
            )
        );
        let back = exact_text(&added, |c| {
            c.sources[0].subvolumes.pop();
        });
        assert_eq!(back, file);
    }

    /// #4, #19: bytes inside a multi-line string are never rewritten, whatever
    /// the file's line endings.
    #[test]
    fn splice_line_endings_inside_multi_line_strings_are_kept() {
        let retire =
            |c: &mut Config| c.sources[0].subvolumes[0].retired = Some("2026-01-01".into());
        let target = "[[target]]\nlabel = \"t0\"\nserials = [\"SERIAL01\"]\nmount = \"/mnt/backup-0\"\n\
                      role = \"primary\"\ndisplay_name = \"\"\"two\r\nlines\"\"\"\n\
                      retention = { daily = 1, weekly = 0, monthly = 0, yearly = 0 }\n";
        let file = format!("{SPLICE_HEAD}{}{target}", src("s", SUB_A));
        let want = spliced(&file, &[("\"a\" }]", "\"a\", retired = \"2026-01-01\" }]")]);
        assert_eq!(exact_text(&file, retire), want);
        // An unknown key's multi-line string, LF inside a CRLF file and CRLF
        // inside an LF file.
        let lf = format!(
            "{}{}{}",
            SPLICE_HEAD.replace(
                "randomized_delay_min = 30\n",
                "randomized_delay_min = 30\nfuture_text = \"\"\"line one\r\nline two\"\"\"\n"
            ),
            src("s", SUB_A),
            tgt(0)
        );
        assert_eq!(
            exact_text(&lf, retire),
            spliced(&lf, &[("\"a\" }]", "\"a\", retired = \"2026-01-01\" }]")])
        );
        let crlf = format!("{SPLICE_HEAD}{}{}", src("s", SUB_A), tgt(0))
            .replace('\n', "\r\n")
            .replace(
                "randomized_delay_min = 30\r\n",
                "randomized_delay_min = 30\r\nfuture_text = \"\"\"line one\nline two\"\"\"\r\n",
            );
        assert_eq!(
            exact_text(&crlf, retire),
            spliced(&crlf, &[("\"a\" }]", "\"a\", retired = \"2026-01-01\" }]")])
        );
    }

    /// #5, #13: an entry whose `[[header]]` is indented takes its indentation
    /// with it, and the comment lines above it stay.
    #[test]
    fn splice_an_indented_header_goes_with_its_entry() {
        let indented = |i: usize| tgt(i).replacen("[[target]]", "  [[target]]", 1);
        let file = format!(
            "{SPLICE_HEAD}{}{}\n{}",
            src("s", SUB_A),
            indented(0),
            indented(1)
        );
        let text = exact_text(&file, |c| {
            c.targets.remove(0);
        });
        assert_eq!(text, spliced(&file, &[(indented(0).as_str(), "")]));
        // The comment above a dropped indented entry stays.
        let file = format!(
            "{SPLICE_HEAD}{}{}\n# the second drive\n{}",
            src("s", SUB_A),
            tgt(0),
            indented(1)
        );
        let text = exact_text(&file, |c| {
            c.targets.remove(1);
        });
        assert_eq!(text, spliced(&file, &[(indented(1).as_str(), "")]));
        // An indented subvolume entry.
        let entries = "  [[source.subvolumes]]\n  name = \"@a\"\n  snapshot_name = \"a\"\n\
                       # note about b\n[[source.subvolumes]]\nname = \"@b\"\nsnapshot_name = \"b\"\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", entries), tgt(0));
        let text = exact_text(&file, |c| {
            c.sources[0].subvolumes.remove(0);
        });
        assert_eq!(
            text,
            spliced(
                &file,
                &[(
                    "  [[source.subvolumes]]\n  name = \"@a\"\n  snapshot_name = \"a\"\n",
                    ""
                )]
            )
        );
    }

    /// #6: two adjacent entries removed in one save leave one blank line where
    /// three would meet.
    #[test]
    fn splice_adjacent_removals_leave_one_blank_line() {
        let file = format!(
            "{SPLICE_HEAD}{}\n{}\n{}\n{}",
            src("s", SUB_A),
            tgt(0),
            tgt(1),
            tgt(2)
        );
        let text = exact_text(&file, |c| {
            c.targets.drain(0..2);
        });
        assert_eq!(
            text,
            spliced(&file, &[(format!("{}\n{}\n", tgt(0), tgt(1)).as_str(), "")])
        );
        // The last source and the first target after it.
        let file = format!(
            "{SPLICE_HEAD}{}\n{}\n{}\n{}",
            src("s", SUB_A),
            src(
                "s1",
                "subvolumes = [{ name = \"@b\", snapshot_name = \"b\" }]\n"
            ),
            tgt(0),
            tgt(1)
        );
        let text = exact_text(&file, |c| {
            c.sources.pop();
            c.targets.remove(0);
        });
        assert_eq!(
            text,
            spliced(
                &file,
                &[(
                    format!(
                        "{}\n{}\n",
                        src(
                            "s1",
                            "subvolumes = [{ name = \"@b\", snapshot_name = \"b\" }]\n"
                        ),
                        tgt(0)
                    )
                    .as_str(),
                    ""
                )]
            )
        );
    }

    fn sub_entry(name: &str) -> String {
        format!("[[source.subvolumes]]\nname = \"@{name}\"\nsnapshot_name = \"{name}\"\n")
    }

    /// #7, #15, #21: a dropped `[[source]]` takes its own entries with it; the
    /// comment lines around it — above its header, under its last entry —
    /// stay where they are.
    #[test]
    fn splice_a_dropped_source_takes_its_entries_and_leaves_the_lines_around_it() {
        let s1 = src(
            "s1",
            "subvolumes = [{ name = \"@b\", snapshot_name = \"b\" }]\n",
        );
        let file = format!(
            "{SPLICE_HEAD}{}\n# about s1\n{s1}{}",
            src("s", &sub_entry("a")),
            tgt(0)
        );
        let (text, warnings) = exact(&file, |c| {
            c.sources.pop();
        });
        assert_eq!(text, spliced(&file, &[(s1.as_str(), "")]));
        assert_eq!(warnings, Vec::<String>::new());
        let s0 = src("s", &sub_entry("a"));
        let file = format!("{SPLICE_HEAD}{s0}# note under @a\n\n{s1}{}", tgt(0));
        let (text, warnings) = exact(&file, |c| {
            c.sources.remove(0);
        });
        assert_eq!(text, spliced(&file, &[(s0.as_str(), "")]));
        assert_eq!(warnings, Vec::<String>::new());
        // #21: the block under a dropped source's last nested line stays.
        let gone = src(
            "gone",
            "[[source.subvolumes]]\nname = \"@old\"\nretired = \"2020-01-01\"\n",
        )
        .replace("/vol", "/vol2");
        let keep = src("keep", &sub_entry("x"));
        let file = format!(
            "{SPLICE_HEAD}{gone}# block under the last line\n\n{keep}{}",
            tgt(0)
        );
        let (text, warnings) = exact(&file, |c| {
            c.sources.remove(0);
        });
        assert_eq!(text, spliced(&file, &[(gone.as_str(), "")]));
        assert_eq!(warnings, Vec::<String>::new());
    }

    /// #8: removing an entry and the last key of the entry before it, in one
    /// save, keeps both comment blocks in their order.
    #[test]
    fn splice_two_removals_keep_the_comment_blocks_in_order() {
        let entries = "[[source.subvolumes]]\nname = \"@a\"\nsnapshot_name = \"a\"\n\
                       retired = \"2026-01-01\"\n# under a\n\n\
                       [[source.subvolumes]]\nname = \"@b\"\nsnapshot_name = \"b\"\n# under b\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", entries), tgt(0));
        let text = exact_text(&file, |c| {
            c.sources[0].subvolumes[0].retired = None;
            c.sources[0].subvolumes.remove(1);
        });
        assert_eq!(
            text,
            spliced(
                &file,
                &[
                    ("retired = \"2026-01-01\"\n", ""),
                    (
                        "[[source.subvolumes]]\nname = \"@b\"\nsnapshot_name = \"b\"\n",
                        ""
                    ),
                ]
            )
        );
    }

    /// #9, #16: replacing or dropping every entry keeps the comment and blank
    /// lines around them; an emptied `[[target]]` writes nothing in its place.
    #[test]
    fn splice_replacing_or_emptying_every_entry_keeps_the_lines_around_them() {
        let entries =
            "\n# above a\n[[source.subvolumes]]\nname = \"@a\"\nsnapshot_name = \"a\"\n# under a\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", entries), tgt(0));
        let text = exact_text(&file, |c| {
            c.sources[0].subvolumes.remove(0);
            c.sources[0].subvolumes.push(sub("@b", "b"));
        });
        assert_eq!(
            text,
            spliced(
                &file,
                &[(
                    "name = \"@a\"\nsnapshot_name = \"a\"\n",
                    "name = \"@b\"\nsnapshot_name = \"b\"\n"
                )]
            )
        );
        let file = format!(
            "{SPLICE_HEAD}{}\n# above t0\n{}# under t0\n",
            src("s", SUB_A),
            tgt(0)
        );
        let text = exact_text(&file, |c| c.targets.clear());
        assert_eq!(text, spliced(&file, &[(tgt(0).as_str(), "")]));
        let file = format!(
            "# file header\n{SPLICE_HEAD}{}\n# about t\n[[target]]\nlabel = \"t\"\n\
             serials = [\"SERIAL01\"]\nmount = \"/mnt/backup-0\"\nrole = \"primary\"\n\
             [target.retention]\ndaily = 7\nweekly = 0\nmonthly = 0\nyearly = 0\n# under t\n\
             \n[email2]\nx = 1\n",
            src("s", SUB_A)
        );
        let text = exact_text(&file, |c| c.targets.clear());
        assert_eq!(
            text,
            spliced(
                &file,
                &[(
                    "[[target]]\nlabel = \"t\"\nserials = [\"SERIAL01\"]\nmount = \"/mnt/backup-0\"\n\
                     role = \"primary\"\n[target.retention]\ndaily = 7\nweekly = 0\nmonthly = 0\n\
                     yearly = 0\n",
                    ""
                )]
            )
        );
    }

    /// #10: removing the first element of a multi-line array leaves the next
    /// one's indentation and the blank line above it.
    #[test]
    fn splice_removing_the_first_element_keeps_the_next_ones_spacing() {
        let line = "subvolumes = [\n  { name = \"@a\", snapshot_name = \"a\" },\n\n    \
                    { name = \"@b\", snapshot_name = \"b\" },\n]\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", line), tgt(0));
        let text = exact_text(&file, |c| {
            c.sources[0].subvolumes.remove(0);
        });
        assert_eq!(
            text,
            spliced(
                &file,
                &[("  { name = \"@a\", snapshot_name = \"a\" },\n", "")]
            )
        );
    }

    /// #11: a key added at the end of a table leaves the line above it, its
    /// trailing whitespace included, as it was.
    #[test]
    fn splice_a_key_added_after_trailing_whitespace_leaves_that_line_alone() {
        let entries = "[[source.subvolumes]]\nname = \"@a\"\nsnapshot_name = \"a\"\t\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", entries), tgt(0));
        let text = exact_text(&file, |c| {
            c.sources[0].subvolumes[0].retired = Some("2026-01-01".into())
        });
        assert_eq!(
            text,
            spliced(
                &file,
                &[(
                    "snapshot_name = \"a\"\t\n",
                    "snapshot_name = \"a\"\t\nretired = \"2026-01-01\"\n"
                )]
            )
        );
    }

    /// #12: a no-op save of a file with no final newline changes nothing; a
    /// key added at its end gets the newline it needs, no more.
    #[test]
    fn splice_a_file_without_a_final_newline_keeps_it_that_way() {
        let file = format!("{SPLICE_HEAD}{}{}", src("s", SUB_A), tgt(0));
        let bare = file.strip_suffix('\n').unwrap().to_string();
        assert_eq!(exact_text(&bare, |_| {}), bare);
        let commented = format!("{file}# eof");
        assert_eq!(exact_text(&commented, |_| {}), commented);
        let text = exact_text(&bare, |c| {
            c.targets[0].mount_uuid = Some("5eed0016-0000-4000-8000-000000000016".into())
        });
        assert_eq!(
            text,
            format!("{bare}\nmount_uuid = \"5eed0016-0000-4000-8000-000000000016\"")
        );
    }

    /// #14, #18: removing a key alone on its line takes that line and nothing
    /// else — commas and `#` inside the strings and comments around it stay.
    #[test]
    fn splice_removing_a_key_never_touches_the_line_above() {
        let clear = |c: &mut Config| c.recovery_os.egress_org = None;
        for above in [
            "note = \"\\\",#\"\n",
            "install_user = \"a\\\",#\"\n",
            "note = \"\\\"\" # list,\n",
            "note = \"\"\"x\"y,#\"\"\"\n",
            "note = '''it's''' # c,\n",
            "note = '''\nfirst,\n#'''\n",
        ] {
            let file = format!(
                "{SPLICE_HEAD}[recovery_os]\n{above}egress_org = \"AS64500\"\n\n{}{}",
                src("s", SUB_A),
                tgt(0)
            );
            assert_eq!(
                exact_text(&file, clear),
                spliced(&file, &[("egress_org = \"AS64500\"\n", "")]),
                "{above:?}"
            );
        }
        // The nightly revive under a multi-line note.
        let entries = "[[source.subvolumes]]\nname = \"@\"\nsnapshot_name = \"root\"\n\
                       [[source.subvolumes]]\nname = \"@x\"\nsnapshot_name = \"x\"\n\
                       note = \"\"\"\nmoved to the new pool,\nsee ticket, #42\"\"\"\n\
                       retired = \"2026-01-01\"\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", entries), tgt(0));
        assert_eq!(
            exact_text(&file, |c| c.sources[0].subvolumes[1].retired = None),
            spliced(&file, &[("retired = \"2026-01-01\"\n", "")])
        );
        // A known key above: the save goes through.
        let known = entries
            .replace("snapshot_name = \"x\"\nnote", "note")
            .replace("retired", "snapshot_name = \"a\\\"b, #c\"\nretired");
        let file = format!("{SPLICE_HEAD}{}{}", src("s", &known), tgt(0));
        assert_eq!(
            exact_text(&file, |c| c.sources[0].subvolumes[1].retired = None),
            spliced(&file, &[("retired = \"2026-01-01\"\n", "")])
        );
        // A legacy serial removed under a comment holding commas.
        let target = "[[target]]\nlabel = \"t0\"\nmount = \"/mnt/backup-0\"\nrole = \"primary\"\n\
                      retention = { daily = 1, weekly = 0, monthly = 0, yearly = 0 }\n\
                      display_name = \"12\\\" bay\" # left, cold,\nserial = \"SERIAL01\"\n";
        let file = format!("{SPLICE_HEAD}{}{target}", src("s", SUB_A));
        assert_eq!(
            exact_text(&file, |c| {
                c.targets[0].serials = vec!["SERIAL01".into(), "SERIAL02".into()]
            }),
            spliced(
                &file,
                &[(
                    "serial = \"SERIAL01\"\n",
                    "serials = [\"SERIAL01\", \"SERIAL02\"]\n"
                )]
            )
        );
        // A key alone on its line inside a multi-line inline entry.
        let line = "subvolumes = [{\n  name = \"@x\",\n  snapshot_name = \"x\\\",#\",\n  \
                    retired = \"2026-10-01\"\n}]\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", line), tgt(0));
        assert_eq!(
            exact_text(&file, |c| c.sources[0].subvolumes[0].retired = None),
            spliced(&file, &[("  retired = \"2026-10-01\"\n", "")])
        );
    }

    /// #17: an entry added to an empty multi-line array keeps the comment on
    /// its `[` line.
    #[test]
    fn splice_an_entry_added_to_an_empty_array_keeps_its_comment() {
        let file = format!(
            "{SPLICE_HEAD}{}{}",
            src("s", "subvolumes = [ # c\n]\n"),
            tgt(0)
        );
        let text = exact_text(&file, |c| {
            c.sources[0].subvolumes.push(SubvolConfig {
                name: "@new".into(),
                snapshot_name: Some("new".into()),
                adopted: Some("2026-11-01".into()),
                ..Default::default()
            })
        });
        assert_eq!(
            text,
            spliced(
                &file,
                &[(
                    "[ # c\n]",
                    "[ # c\n{ name = \"@new\", snapshot_name = \"new\", adopted = \"2026-11-01\" }\n]"
                )]
            )
        );
    }

    /// #20 and the m0pr.2 warning: a dropped element is named by its index
    /// in the file, and so is the element that holds it.
    #[test]
    fn splice_a_nested_warning_names_its_parent_by_its_place_in_the_file() {
        let gone = "[[source]]\nlabel = \"gone\"\nvolume = \"/vol2\"\ndevice = \"/dev/sdb\"\n\
                    snapshot_dir = \".btrbk-snapshots\"\ntarget_subdirs = []\ntarget_labels = []\n\
                    [[source.subvolumes]]\nname = \"@old\"\nsnapshot_name = \"old\"\n\
                    retired = \"2020-01-01\"\nfuture = \"g\"\n";
        let keep = src(
            "keep",
            "[[source.subvolumes]]\nname = \"@\"\nsnapshot_name = \"root\"\n\
             [[source.subvolumes]]\nname = \"@old\"\nsnapshot_name = \"old\"\n\
             retired = \"2020-01-01\"\nfuture = \"k\"\n",
        );
        let file = format!("{SPLICE_HEAD}{gone}{keep}{}", tgt(0));
        let (text, warnings) = exact(&file, |c| {
            c.sources.remove(0);
            c.sources[0].subvolumes.remove(1);
        });
        let tail = "; a renamed element is written fresh, so anything it should keep must be \
                    added to it again";
        assert_eq!(
            warnings,
            [
                format!(
                    "source[0] \"gone\" is no longer in the config and was removed with keys \
                     this version does not read (subvolumes[0].future){tail}"
                ),
                format!(
                    "source[1].subvolumes[1] \"@old\" is no longer in the config and was \
                     removed with keys this version does not read (future){tail}"
                ),
            ]
        );
        assert_eq!(
            text,
            spliced(
                &file,
                &[
                    (gone, ""),
                    (
                        "[[source.subvolumes]]\nname = \"@old\"\nsnapshot_name = \"old\"\n\
                         retired = \"2020-01-01\"\nfuture = \"k\"\n",
                        ""
                    ),
                ]
            )
        );
    }

    /// 5237.1: dotted keys inside an inline table are read, edited and pinned
    /// where they are.
    #[test]
    fn splice_dotted_keys_inside_an_inline_table() {
        let target = "target = [{ label = \"t0\", serials = [\"SERIAL01\"], mount = \"/mnt/backup-0\", \
                      role = \"primary\", retention.daily = 7 }]\n";
        let file = format!("{target}{SPLICE_HEAD}{}", src("s", SUB_A));
        assert_eq!(
            exact_text(&file, |_| {}),
            spliced(
                &file,
                &[(
                    "retention.daily = 7 }",
                    "retention.daily = 7, retention.weekly = 0, retention.monthly = 0, \
                     retention.yearly = 0 }"
                )]
            )
        );
        let file = format!(
            "recovery_os = {{ egress_org = \"AS64500\", future.x = 1 }}\n{SPLICE_HEAD}{}{}",
            src("s", SUB_A),
            tgt(0)
        );
        assert_eq!(
            exact_text(&file, |c| c.recovery_os.egress_org = None),
            spliced(&file, &[("egress_org = \"AS64500\", ", "")])
        );
        assert_eq!(
            exact_text(&file, |c| c.recovery_os.max_age_days = 30),
            spliced(
                &file,
                &[("future.x = 1 }", "future.x = 1, max_age_days = 30 }")]
            )
        );
    }

    /// 5237.2: the comment line above a removed plain-string entry stays,
    /// with or without a comma after the entry.
    #[test]
    fn splice_a_comment_above_a_removed_plain_entry_stays() {
        for b in ["  \"@b\",\n", "  \"@b\"\n"] {
            let exclude = format!("exclude = [\n  \"@a\",\n  # keep b out: scratch\n{b}]\n");
            let file = format!("{SPLICE_HEAD}{}{}", src("s", SUB_A), tgt(0)).replace(
                "[subvolumes]\nexclude = []\n",
                &format!("[subvolumes]\n{exclude}"),
            );
            let (text, warnings) = exact(&file, |c| c.subvolumes.exclude.retain(|e| e != "@b"));
            assert_eq!(text, spliced(&file, &[(b, "")]));
            assert_eq!(warnings, Vec::<String>::new());
        }
    }

    /// 5237.3: interleaved dotted keys keep their order on any save.
    #[test]
    fn splice_interleaved_dotted_keys_keep_their_order() {
        let target = "[[target]]\nlabel = \"t0\"\nretention.daily = 7\nserials = [\"SERIAL01\"]\n\
                      retention.weekly = 4\nmount = \"/mnt/backup-0\"\nretention.monthly = 1\n\
                      role = \"primary\"\nretention.yearly = 0\n";
        let file = format!("{SPLICE_HEAD}{}{target}", src("s", SUB_A));
        assert_eq!(
            exact_text(&file, |c| c.schedule.full = "Sat 05:00".into()),
            spliced(&file, &[("Sun 04:00", "Sat 05:00")])
        );
        assert_eq!(
            exact_text(&file, |c| c.targets[0].retention.weekly = 5),
            spliced(&file, &[("retention.weekly = 4", "retention.weekly = 5")])
        );
    }

    /// m0pr.3: the comment block under a table's only value stays when that
    /// value is removed.
    #[test]
    fn splice_a_comment_under_a_tables_only_value_stays() {
        let file = format!(
            "{SPLICE_HEAD}[recovery_os]\negress_org = \"AS64500\"\n# about recovery\n\n{}{}",
            src("s", SUB_A),
            tgt(0)
        );
        assert_eq!(
            exact_text(&file, |c| c.recovery_os.egress_org = None),
            spliced(&file, &[("egress_org = \"AS64500\"\n", "")])
        );
    }

    /// A save that changes nothing writes the bytes it read: the operator's
    /// file, CRLF, a byte-order mark.
    #[test]
    fn splice_a_no_op_save_is_byte_identical() {
        let file = pinned(OPERATOR_FILE);
        assert_eq!(exact_text(&file, |_| {}), file);
        let crlf = file.replace('\n', "\r\n");
        assert_eq!(exact_text(&crlf, |_| {}), crlf);
        let bom = format!("\u{feff}{file}");
        assert_eq!(exact_text(&bom, |_| {}), bom);
    }

    /// A key added to a CRLF file ends its line with CRLF; nothing else moves.
    #[test]
    fn splice_a_key_added_to_a_crlf_file_is_crlf() {
        let file = format!("{SPLICE_HEAD}{}{}", src("s", SUB_A), tgt(0)).replace('\n', "\r\n");
        let text = exact_text(&file, |c| {
            c.targets[0].mount_uuid = Some("5eed0016-0000-4000-8000-000000000016".into())
        });
        assert_eq!(
            text,
            format!("{file}mount_uuid = \"5eed0016-0000-4000-8000-000000000016\"\r\n")
        );
    }

    /// The m0pr.2 warning: an element dropped with keys this version does not
    /// read is named with them; one dropped with only comments of its own
    /// says so; the lines around it are not its own.
    #[test]
    fn splice_a_dropped_element_with_unknown_keys_or_its_own_comments_is_warned() {
        let tail = "; a renamed element is written fresh, so anything it should keep must be \
                    added to it again";
        let b = "[[target]]\nlabel = \"b\"\nmount = \"/mnt/b\"\nrole = \"mirror\"\nfuture = \"of b\"\n\
                 [target.retention]\ndaily = 3\nweekly = 0\nmonthly = 0\nyearly = 0\nfuture_r = 1\n";
        let file = format!("{SPLICE_HEAD}{}{}{b}", src("s", SUB_A), tgt(0));
        let (text, warnings) = exact(&file, |c| {
            c.targets.pop();
        });
        assert_eq!(text, spliced(&file, &[(b, "")]));
        assert_eq!(
            warnings,
            [format!(
                "target[1] \"b\" is no longer in the config and was removed with keys this \
                 version does not read (future, retention.future_r){tail}"
            )]
        );
        // Comments: on the header line, above a key, above a sub-table, in an
        // array it holds.
        for own in [
            b.replace("[[target]]\n", "[[target]] # the mirror\n"),
            b.replace("mount = ", "# where b mounts\nmount = "),
            b.replace("[target.retention]", "\n# three days\n[target.retention]"),
            b.replace("role = ", "serials = [\n    \"S1\", # bay 1\n]\nrole = "),
        ] {
            let own = own
                .replace("future = \"of b\"\n", "")
                .replace("future_r = 1\n", "");
            let file = format!("{SPLICE_HEAD}{}{}{own}", src("s", SUB_A), tgt(0));
            let (text, warnings) = exact(&file, |c| {
                c.targets.pop();
            });
            assert_eq!(text, spliced(&file, &[(own.as_str(), "")]));
            assert_eq!(
                warnings,
                [format!(
                    "target[1] \"b\" is no longer in the config and was removed with its \
                     comments{tail}"
                )],
                "{own}"
            );
        }
        // Around it: above its header, under its last line. Not warned.
        let plain = b
            .replace("future = \"of b\"\n", "")
            .replace("future_r = 1\n", "");
        let file = format!(
            "{SPLICE_HEAD}{}{}\n# the mirror\n{plain}# under b\n",
            src("s", SUB_A),
            tgt(0)
        );
        let (text, warnings) = exact(&file, |c| {
            c.targets.pop();
        });
        assert_eq!(text, spliced(&file, &[(plain.as_str(), "")]));
        assert_eq!(warnings, Vec::<String>::new());
        // Inline entries: unknown key, a comment after its comma, inside it;
        // a bare string counts in the index; a key the loader reads but the
        // serialiser omits (`after_index = false`) is known.
        for (line, gone, warned) in [
            (
                "subvolumes = [\"@\", {name = \"@x\", future = 1}]\n",
                ", {name = \"@x\", future = 1}",
                Some("keys this version does not read (future)"),
            ),
            (
                "subvolumes = [\n    \"@\",\n    \"@x\", # scratch\n]\n",
                "    \"@x\", # scratch\n",
                Some("its comments"),
            ),
            (
                "subvolumes = [\"@\", { name = \"@x\", # scratch\n    snapshot_name = \"x\" }]\n",
                ", { name = \"@x\", # scratch\n    snapshot_name = \"x\" }",
                Some("its comments"),
            ),
            (
                "subvolumes = [\"@\", { name = \"@x\", after_index = false }]\n",
                ", { name = \"@x\", after_index = false }",
                None,
            ),
        ] {
            let file = format!("{SPLICE_HEAD}{}{}", src("s", line), tgt(0));
            let (text, warnings) = exact(&file, |c| {
                c.sources[0].subvolumes.pop();
            });
            // The bare `"@"` kept is pinned into a table at the same time.
            let want = line.replace(gone, "").replacen(
                "\"@\"",
                "{ name = \"@\", snapshot_name = \"root\" }",
                1,
            );
            assert_eq!(text, spliced(&file, &[(line, want.as_str())]), "{line}");
            match warned {
                Some(what) => assert_eq!(
                    warnings,
                    [format!(
                        "source[0].subvolumes[1] \"@x\" is no longer in the config and was \
                         removed with {what}{tail}"
                    )],
                    "{line}"
                ),
                None => assert_eq!(warnings, Vec::<String>::new(), "{line}"),
            }
        }
        // A dropped source's own entries' unknown keys, a bare string counted.
        let s2 = src(
            "s2",
            "subvolumes = [\"@a\", { name = \"@x\", snapshot_name = \"x\", future = 1 }]\n",
        )
        .replace("/vol", "/vol2");
        let file = format!("{SPLICE_HEAD}{}{s2}{}", src("s", SUB_A), tgt(0));
        let (text, warnings) = exact(&file, |c| {
            c.sources.pop();
        });
        assert_eq!(text, spliced(&file, &[(s2.as_str(), "")]));
        assert_eq!(
            warnings,
            [format!(
                "source[1] \"s2\" is no longer in the config and was removed with keys this \
                 version does not read (subvolumes[1].future){tail}"
            )]
        );
    }

    /// A save that fails to write reports nothing; once the new file is in
    /// place the warning is reported, even when flushing its directory then
    /// fails.
    #[test]
    fn splice_warnings_are_reported_only_once_the_file_is_replaced() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, directory modes do not deny it");
            return;
        }
        let file = format!(
            "{SPLICE_HEAD}{}{}[[target]]\nlabel = \"b\"\nmount = \"/mnt/b\"\nrole = \"mirror\"\n\
             future = 1\nretention = {{ daily = 3, weekly = 0, monthly = 0, yearly = 0 }}\n",
            src("s", SUB_A),
            tgt(0)
        );
        for (mode, saved) in [(0o555, false), (0o333, true)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            std::fs::write(&path, &file).unwrap();
            let mut cfg = Config::load(&path).unwrap();
            cfg.targets.pop();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            let mut warnings = Vec::new();
            let result = cfg.save_reporting(&path, &|_| {}, &mut |w| warnings.push(w));
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(
                result.is_err(),
                "mode {mode:o}: the save was expected to fail"
            );
            assert_eq!(
                warnings.len(),
                usize::from(saved),
                "mode {mode:o}: {warnings:?}"
            );
        }
    }

    /// Renames are never inferred: an added element never takes a removed
    /// one's unknown keys.
    #[test]
    fn splice_a_rename_never_carries_keys_to_the_added_element() {
        let entries = "[[source.subvolumes]]\nname = \"@a\"\nsnapshot_name = \"a\"\nfuture = 1\n";
        let file = format!("{SPLICE_HEAD}{}{}", src("s", entries), tgt(0));
        let (text, warnings) = exact(&file, |c| c.sources[0].subvolumes[0].name = "@b".into());
        assert_eq!(
            text,
            spliced(
                &file,
                &[(
                    "name = \"@a\"\nsnapshot_name = \"a\"\nfuture = 1\n",
                    "name = \"@b\"\nsnapshot_name = \"a\"\n"
                )]
            )
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }
}
