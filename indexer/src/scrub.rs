//! Scheduled BTRFS scrub engine for the DAS backup filesystems.
//!
//! The DAS targets are unmounted between backup runs, so a calendar-driven
//! `btrfs-scrub@.timer` cannot work here: it would find no filesystem. This
//! module owns the whole lifecycle instead — mount, scrub, parse the result,
//! unmount — for each configured scrub target **in sequence** (all three DAS
//! filesystems hang off one USB bus; running them in parallel would starve the
//! bus and inflate an already ~14 h pass).
//!
//! # The two rules that motivated this module
//!
//! Both come out of the 2026-07-27 `system-recovery-A` investigation, where a
//! scrub that never completed was read as healthy for 64 days:
//!
//! 1. **`finished` is not the same as `aborted`.** The 2026-05-24 record showed
//!    `Status: aborted`, `super=3`, `Total to scrub: 0.00B` and was treated as
//!    fine. In the on-disk record an aborted scrub is simply
//!    `canceled:0|finished:0` — there is no `aborted` key to look for, so any
//!    parser that only checks error counters silently passes it. See
//!    [`ScrubOutcome`]: only [`ScrubOutcome::Finished`] with zero error
//!    counters is a pass.
//! 2. **Everything is resolved by filesystem UUID, never by mount path.**
//!    `btrfs scrub status <path>` on an *unmounted* path silently reports the
//!    record of whatever filesystem backs that path (observed: the NVMe root's
//!    record, for a query about a DAS drive). Status is therefore always read
//!    from `/var/lib/btrfs/scrub.status.<fsuuid>`, mounts are performed with
//!    `UUID=<fsuuid>`, and every mount is verified with
//!    `findmnt -o UUID` before a scrub is started against it.
//!
//! A third guard falls out of the same incident: after the scrub, the record's
//! last start-or-resume time must be at or after this filesystem's scrub start.
//! A stale record left by an earlier scrub is reported as a failure rather than
//! mistaken for a result.
//!
//! # Vocabulary warning: `aborted` here vs. in `btrfs scrub status`
//!
//! The CLI's `Status:` line and this module's [`ScrubOutcome`] do **not** use
//! the same words for the same states (verified on btrfs-progs v7.1):
//!
//! | record flags | `btrfs scrub status` prints | [`ScrubOutcome`] |
//! |---|---|---|
//! | `finished:1 canceled:0` | `finished` | `Finished` |
//! | `canceled:1` | `aborted` | `Canceled` |
//! | `finished:0 canceled:0`, nothing running | `interrupted` | `Aborted` |
//! | `finished:0 canceled:0`, scrub live | `running` | `Aborted` |
//!
//! This module keys on the raw flags, never on the rendered word, because the
//! rendering has changed across btrfs-progs versions. Both non-`Finished`
//! states are failures, so behavior is unaffected — but do not expect the
//! module's word and the CLI's word to match when comparing by eye.
//!
//! # Clearing a stale aborted record (`-f`)
//!
//! `btrfs-progs` can read a saved record whose device row is neither
//! `finished` nor `canceled` as evidence of a live scrub, and refuse to start
//! ("useful when scrub status file is damaged and reports a running scrub
//! although it is not", `man btrfs-scrub` on `-f`). Left unhandled, one
//! aborted pass could wedge that filesystem's scrubs indefinitely.
//! [`decide_scrub_start_mode`] adds `-f` **only** when the prior record is
//! `Aborted` *and* the kernel confirms nothing is running — never
//! unconditionally, because these locks cannot stop an operator from starting
//! a scrub by hand and force-restarting theirs would discard hours of work.
//!
//! # Operational notes
//!
//! * **Unmount failure does not cascade.** An unmount that fails (after one
//!   retry) fails that filesystem and is reported, but the pass continues to
//!   the next target and the locks are still released at the end. The
//!   consequence of a target left mounted is caught downstream:
//!   `backup-run.sh`'s `verify_targets_before_btrbk` checks the mounted
//!   filesystem's UUID against the config, so a stray mount is surfaced by the
//!   next backup rather than silently used.
//! * **Double mounts alongside udisks are expected and tolerated.** The 22 TB
//!   array is often also mounted at `/run/media/bosco/…` by the file manager —
//!   that is what triggers `btrfs-scrub-resume-das-22tb.service` and is
//!   deliberately not cleaned up. BTRFS permits the same filesystem at several
//!   mount points; a scrub is per-filesystem, not per-mount, so it makes no
//!   difference to this engine. If the configured mountpoint already holds the
//!   *expected* UUID the engine reuses it and does **not** unmount it on the
//!   way out (it only unmounts what it mounted).
//!
//! # On-disk status record format
//!
//! `/var/lib/btrfs/scrub.status.<fsuuid>` is written by `btrfs-progs`:
//!
//! ```text
//! scrub status:1
//! <fsuuid>:<devid>|data_extents_scrubbed:N|…|duration:N|canceled:N|finished:N
//! ```
//!
//! One row per device (a RAID-1 target has two). [`parse_scrub_status`] rejects
//! a file whose rows do not carry the UUID that was asked for, and aggregates
//! the per-device rows into one [`ScrubStatusRecord`].
//!
//! # Locking (two locks, acquired singleton → maintenance)
//!
//! * `/run/das-scrub.lock` — non-blocking. Held means another scrub pass is
//!   already running, so this invocation **skips** (logs and returns
//!   [`PassStatus::Skipped`]); it never queues.
//! * `/run/das-maintenance.lock` — blocking, shared with `backup-run.sh`. Held
//!   for the entire pass, including this module's own mounts and unmounts, so a
//!   backup can never unmount a filesystem out from under a running scrub.
//!   Contention defers the scrub, it never cancels it.
//!
//! Both locks live in the engine, not in the systemd unit, so a manual
//! `btrdasd scrub run` is covered identically. `/run` is tmpfs, so no lock can
//! go stale across a boot. Release order is the reverse of acquisition.
//!
//! # State file schema — `/var/lib/das-backup/scrub-state.json`
//!
//! Written atomically (temp file + rename), root-owned, mode `0644` so health
//! checks and the GUI can read it **without any DAS filesystem mounted**. This
//! is the contract consumed by the health integration:
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "last_pass": {
//!     "started_epoch": 1785182081,
//!     "finished_epoch": 1785232755,
//!     "targets_attempted": 3,
//!     "targets_failed": 1,
//!     "success": false
//!   },
//!   "filesystems": {
//!     "60b05268-7f8f-47b5-a38a-752576a1172a": {
//!       "target_label": "system-recovery-A-2tb",
//!       "mountpoint": "/mnt/backup-system-recovery-A",
//!       "last_success_epoch": 1785188355,
//!       "last_attempt": {
//!         "outcome": "finished",
//!         "ok": true,
//!         "started_epoch": 1785182081,
//!         "finished_epoch": 1785188355,
//!         "duration_secs": 6274,
//!         "bytes_scrubbed": 1094804803584,
//!         "error_total": 0,
//!         "counters": { "read_errors": 0, "csum_errors": 0, "verify_errors": 0,
//!                       "super_errors": 0, "malloc_errors": 0,
//!                       "uncorrectable_errors": 0, "corrected_errors": 0,
//!                       "no_csum": 0, "csum_discards": 0 },
//!         "messages": []
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! Schema notes for consumers:
//!
//! * Keys of `filesystems` are **filesystem UUIDs**, not labels or mountpoints.
//!   `target_label` is carried alongside for display only.
//! * `last_attempt.outcome` is one of `finished`, `canceled`, `aborted`, or
//!   `error` (the pass could not get far enough to produce a record at all).
//!   `ok` is the single authoritative pass/fail bit: `outcome == "finished"`
//!   **and** no error counters **and** no engine-level errors.
//! * `last_success_epoch` is carried forward across failed passes — it is the
//!   field to age against `scrub.warn_age_days` / `scrub.fail_age_days`. It is
//!   `null` until the first successful scrub is recorded.
//! * `counters.no_csum` and `counters.csum_discards` are informational and are
//!   excluded from `error_total` (a healthy filesystem with nodatacow files
//!   reports millions of `no_csum`).
//! * A skipped pass (singleton lock held) does **not** write the state file, so
//!   a skip can never look like a completed scrub.
//! * `schema_version` is `1`. Readers must tolerate unknown extra fields.
//!
//! # Test-only environment overrides
//!
//! `DAS_BTRFS_STATUS_DIR` and `DAS_SCRUB_STATE` relocate the btrfs status
//! directory and the state file, mirroring the `PBRIDGE_CONF` override in
//! [`crate::report`]. Production leaves both unset.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::mount::partition_device;
use crate::progress::{LogLevel, ProgressCallback};

/// Non-blocking singleton lock — a second scrub pass skips rather than queues.
pub const SCRUB_LOCK_PATH: &str = "/run/das-scrub.lock";
/// Blocking maintenance lock, shared with `backup-run.sh`.
pub const MAINTENANCE_LOCK_PATH: &str = "/run/das-maintenance.lock";
/// Default directory holding `scrub.status.<fsuuid>` records.
pub const BTRFS_STATUS_DIR: &str = "/var/lib/btrfs";
/// Default path of the persisted scrub state consumed by health checks.
pub const SCRUB_STATE_PATH: &str = "/var/lib/das-backup/scrub-state.json";
/// Schema version written to (and expected in) the state file.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// How long to wait on the maintenance lock before announcing the wait.
const LOCK_WAIT_ANNOUNCE_SECS: u64 = 5;
/// Poll interval while a scrub child process runs.
const SCRUB_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How often to log scrub progress while waiting.
const SCRUB_PROGRESS_LOG_SECS: u64 = 60;
/// Slack allowed between a filesystem's scrub start and its record's `t_start`
/// before the record is treated as stale (clock granularity, btrfs recording
/// the start a moment before the child is observed to have launched).
const RECORD_FRESHNESS_SLACK_SECS: i64 = 120;
/// How often to repeat the maintenance-lock wait notice, so a pass blocked
/// behind a long backup does not look hung.
const LOCK_WAIT_REANNOUNCE_SECS: u64 = 900;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that abort a scrub pass outright, or that prevent a status record
/// from being interpreted.
#[derive(Debug)]
pub enum ScrubError {
    /// `scrub.targets` is empty — nothing to do.
    NoTargets,
    /// A lock file could not be opened or locked.
    Lock { path: String, detail: String },
    /// The state file could not be read or written.
    State { path: String, detail: String },
    /// The state file holds unparseable JSON. Distinct from
    /// [`ScrubError::State`] because the two demand opposite handling: a
    /// corrupt file may be quarantined and replaced, whereas an
    /// unreadable-but-intact file must never be overwritten. Reporting it does
    /// **not** modify anything — quarantine is [`quarantine_state_file`]'s job,
    /// so that reading state stays a pure, unprivileged-safe operation.
    StateCorrupt { path: String, detail: String },
    /// The status record file is missing.
    StatusMissing { path: String },
    /// The status record file could not be read.
    StatusIo { path: String, detail: String },
    /// The record lacks the `scrub status:<version>` header line.
    StatusMissingHeader,
    /// The record declares a format version this parser does not understand.
    StatusUnsupportedVersion { version: String },
    /// The record's rows are for a different filesystem than the one asked
    /// for — the guard against path-resolved status leaking in.
    StatusUuidMismatch { expected: String, found: String },
    /// The record has a header but no device rows.
    StatusNoDevices { fsuuid: String },
    /// A device row could not be parsed.
    StatusMalformed { detail: String },
}

impl fmt::Display for ScrubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTargets => write!(f, "No scrub targets configured ([scrub].targets is empty)"),
            Self::Lock { path, detail } => write!(f, "Lock '{path}': {detail}"),
            Self::State { path, detail } => write!(f, "Scrub state '{path}': {detail}"),
            Self::StateCorrupt { path, detail } => {
                write!(f, "Scrub state '{path}' is unparseable: {detail}")
            }
            Self::StatusMissing { path } => {
                write!(f, "No scrub status record at '{path}'")
            }
            Self::StatusIo { path, detail } => {
                write!(f, "Cannot read scrub status record '{path}': {detail}")
            }
            Self::StatusMissingHeader => {
                write!(f, "Scrub status record has no 'scrub status:<n>' header")
            }
            Self::StatusUnsupportedVersion { version } => {
                write!(f, "Unsupported scrub status format version '{version}'")
            }
            Self::StatusUuidMismatch { expected, found } => write!(
                f,
                "Scrub status record is for filesystem '{found}', expected '{expected}'"
            ),
            Self::StatusNoDevices { fsuuid } => {
                write!(f, "Scrub status record for '{fsuuid}' has no device rows")
            }
            Self::StatusMalformed { detail } => {
                write!(f, "Malformed scrub status record: {detail}")
            }
        }
    }
}

impl std::error::Error for ScrubError {}

// ---------------------------------------------------------------------------
// Status record parsing
// ---------------------------------------------------------------------------

/// Terminal state of a scrub, derived from the `canceled` / `finished` flags.
///
/// `btrfs-progs` sets `canceled:1` when a scrub was cancelled by request (it
/// also sets `finished:1` in that case, so `canceled` is checked first), and
/// `finished:1` when a scrub ran to completion. A scrub that died — the
/// filesystem vanished, the machine rebooted, the USB link dropped — leaves
/// `canceled:0|finished:0` behind and renders as `Status: aborted` in
/// `btrfs scrub status` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrubOutcome {
    /// Ran to completion.
    Finished,
    /// Cancelled by request.
    Canceled,
    /// Died without completing — the 2026-05-24 failure mode.
    Aborted,
}

impl ScrubOutcome {
    /// Lowercase wire name, as written to the state file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Canceled => "canceled",
            Self::Aborted => "aborted",
        }
    }
}

impl fmt::Display for ScrubOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error counters from a scrub status record, summed across devices.
///
/// `no_csum` and `csum_discards` are *not* errors and are excluded from
/// [`ScrubCounters::error_total`]: a filesystem holding nodatacow files
/// legitimately reports millions of `no_csum` blocks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrubCounters {
    pub read_errors: u64,
    pub csum_errors: u64,
    pub verify_errors: u64,
    pub super_errors: u64,
    pub malloc_errors: u64,
    pub uncorrectable_errors: u64,
    /// Damage that scrub repaired from the other RAID-1 copy. Counted as an
    /// error: the repair succeeded, but the underlying rot is real and the
    /// operator needs to see it.
    pub corrected_errors: u64,
    /// Informational — blocks without checksums (nodatacow). Not an error.
    pub no_csum: u64,
    /// Informational — discarded checksums. Not an error.
    pub csum_discards: u64,
}

impl ScrubCounters {
    /// Sum of the counters that represent real damage.
    pub fn error_total(&self) -> u64 {
        self.read_errors
            + self.csum_errors
            + self.verify_errors
            + self.super_errors
            + self.malloc_errors
            + self.uncorrectable_errors
            + self.corrected_errors
    }

    /// Whether any damage counter is non-zero.
    pub fn has_errors(&self) -> bool {
        self.error_total() > 0
    }

    /// Human-readable summary of only the non-zero damage counters.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (name, value) in [
            ("read", self.read_errors),
            ("csum", self.csum_errors),
            ("verify", self.verify_errors),
            ("super", self.super_errors),
            ("malloc", self.malloc_errors),
            ("uncorrectable", self.uncorrectable_errors),
            ("corrected", self.corrected_errors),
        ] {
            if value > 0 {
                parts.push(format!("{name}={value}"));
            }
        }
        if parts.is_empty() {
            "no errors".to_string()
        } else {
            parts.join(" ")
        }
    }

    fn add(&mut self, other: &ScrubCounters) {
        self.read_errors += other.read_errors;
        self.csum_errors += other.csum_errors;
        self.verify_errors += other.verify_errors;
        self.super_errors += other.super_errors;
        self.malloc_errors += other.malloc_errors;
        self.uncorrectable_errors += other.uncorrectable_errors;
        self.corrected_errors += other.corrected_errors;
        self.no_csum += other.no_csum;
        self.csum_discards += other.csum_discards;
    }
}

/// One device row (`<fsuuid>:<devid>|…`) of a status record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubDeviceStatus {
    pub devid: u64,
    pub counters: ScrubCounters,
    pub data_bytes_scrubbed: u64,
    pub tree_bytes_scrubbed: u64,
    /// Unix epoch when this device's scrub started (0 if the field is absent).
    pub t_start: i64,
    /// Unix epoch when an interrupted scrub was resumed, 0 if never resumed.
    /// Freshness is judged against `max(t_start, t_resumed)` — a resumed scrub
    /// legitimately keeps its original `t_start`.
    pub t_resumed: i64,
    pub duration_secs: u64,
    pub canceled: bool,
    pub finished: bool,
}

impl ScrubDeviceStatus {
    /// Terminal state for this device.
    pub fn outcome(&self) -> ScrubOutcome {
        // `canceled` is checked first: btrfs-progs sets `finished:1` alongside
        // `canceled:1` on a cancelled scrub (seen in the real 2026-05-06
        // record for filesystem 46ffbd7c-…).
        if self.canceled {
            ScrubOutcome::Canceled
        } else if self.finished {
            ScrubOutcome::Finished
        } else {
            ScrubOutcome::Aborted
        }
    }
}

/// A whole `scrub.status.<fsuuid>` record, one entry per device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubStatusRecord {
    pub fsuuid: String,
    pub devices: Vec<ScrubDeviceStatus>,
}

impl ScrubStatusRecord {
    /// Aggregate outcome across devices — the worst wins, so a RAID-1 pair
    /// where one leg aborted is reported as aborted.
    pub fn outcome(&self) -> ScrubOutcome {
        if self
            .devices
            .iter()
            .any(|d| d.outcome() == ScrubOutcome::Aborted)
        {
            ScrubOutcome::Aborted
        } else if self
            .devices
            .iter()
            .any(|d| d.outcome() == ScrubOutcome::Canceled)
        {
            ScrubOutcome::Canceled
        } else {
            ScrubOutcome::Finished
        }
    }

    /// Error counters summed over all devices.
    pub fn counters(&self) -> ScrubCounters {
        let mut total = ScrubCounters::default();
        for d in &self.devices {
            total.add(&d.counters);
        }
        total
    }

    /// Total bytes read by the scrub (data + metadata, all devices).
    pub fn bytes_scrubbed(&self) -> u64 {
        self.devices
            .iter()
            .map(|d| d.data_bytes_scrubbed + d.tree_bytes_scrubbed)
            .sum()
    }

    /// Earliest non-zero device start time, or 0 if none recorded.
    pub fn t_start(&self) -> i64 {
        self.devices
            .iter()
            .map(|d| d.t_start)
            .filter(|t| *t > 0)
            .min()
            .unwrap_or(0)
    }

    /// Latest moment any device began or resumed scrubbing — the anchor for
    /// the freshness check. A resumed scrub keeps its original `t_start`, so
    /// judging freshness on `t_start` alone would wrongly call it stale.
    pub fn last_activity_start(&self) -> i64 {
        self.devices
            .iter()
            .map(|d| d.t_start.max(d.t_resumed))
            .max()
            .unwrap_or(0)
    }

    /// Longest device duration — the wall time the pass actually spent.
    pub fn duration_secs(&self) -> u64 {
        self.devices
            .iter()
            .map(|d| d.duration_secs)
            .max()
            .unwrap_or(0)
    }

    /// Epoch at which the last device stopped.
    pub fn finished_epoch(&self) -> i64 {
        let start = self.t_start();
        if start == 0 {
            0
        } else {
            start + self.duration_secs() as i64
        }
    }

    /// True only for a completed scrub with no damage counters — the single
    /// gate that the 2026-05-24 aborted record has to fail.
    pub fn is_clean(&self) -> bool {
        self.outcome() == ScrubOutcome::Finished && !self.counters().has_errors()
    }
}

/// Parse a `scrub.status.<fsuuid>` record, requiring it to belong to
/// `expected_fsuuid`.
///
/// The UUID check is not a formality: it is what stops a record resolved
/// through a mount path — the 2026-07-27 failure, where an unmounted DAS path
/// yielded the NVMe root's record — from ever being attributed to a DAS
/// filesystem.
pub fn parse_scrub_status(
    content: &str,
    expected_fsuuid: &str,
) -> Result<ScrubStatusRecord, ScrubError> {
    let mut lines = content.lines().filter(|l| !l.trim().is_empty());

    let header = lines.next().ok_or(ScrubError::StatusMissingHeader)?.trim();
    let version = header
        .strip_prefix("scrub status:")
        .ok_or(ScrubError::StatusMissingHeader)?
        .trim();
    if version != "1" {
        return Err(ScrubError::StatusUnsupportedVersion {
            version: version.to_string(),
        });
    }

    let mut devices = Vec::new();
    for line in lines {
        let line = line.trim();
        let mut fields = line.split('|');
        let head = fields.next().unwrap_or_default();
        // Head is `<fsuuid>:<devid>`; the UUID itself contains no ':'.
        let (uuid, devid_str) =
            head.rsplit_once(':')
                .ok_or_else(|| ScrubError::StatusMalformed {
                    detail: format!("device row has no '<fsuuid>:<devid>' prefix: '{head}'"),
                })?;
        if !uuid.eq_ignore_ascii_case(expected_fsuuid) {
            return Err(ScrubError::StatusUuidMismatch {
                expected: expected_fsuuid.to_string(),
                found: uuid.to_string(),
            });
        }
        let devid: u64 = devid_str.parse().map_err(|_| ScrubError::StatusMalformed {
            detail: format!("device id '{devid_str}' is not a number"),
        })?;

        let mut dev = ScrubDeviceStatus {
            devid,
            counters: ScrubCounters::default(),
            data_bytes_scrubbed: 0,
            tree_bytes_scrubbed: 0,
            t_start: 0,
            t_resumed: 0,
            duration_secs: 0,
            canceled: false,
            finished: false,
        };

        for field in fields {
            let Some((key, value)) = field.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            // Unknown keys are ignored so a btrfs-progs that adds counters
            // does not break parsing; the format version guards real changes.
            let num = value.parse::<u64>().ok();
            match key {
                "read_errors" => dev.counters.read_errors = num.unwrap_or(0),
                "csum_errors" => dev.counters.csum_errors = num.unwrap_or(0),
                "verify_errors" => dev.counters.verify_errors = num.unwrap_or(0),
                "super_errors" => dev.counters.super_errors = num.unwrap_or(0),
                "malloc_errors" => dev.counters.malloc_errors = num.unwrap_or(0),
                "uncorrectable_errors" => dev.counters.uncorrectable_errors = num.unwrap_or(0),
                "corrected_errors" => dev.counters.corrected_errors = num.unwrap_or(0),
                "no_csum" => dev.counters.no_csum = num.unwrap_or(0),
                "csum_discards" => dev.counters.csum_discards = num.unwrap_or(0),
                "data_bytes_scrubbed" => dev.data_bytes_scrubbed = num.unwrap_or(0),
                "tree_bytes_scrubbed" => dev.tree_bytes_scrubbed = num.unwrap_or(0),
                "t_start" => dev.t_start = value.parse::<i64>().unwrap_or(0),
                "t_resumed" => dev.t_resumed = value.parse::<i64>().unwrap_or(0),
                "duration" => dev.duration_secs = num.unwrap_or(0),
                "canceled" => dev.canceled = num.unwrap_or(0) != 0,
                "finished" => dev.finished = num.unwrap_or(0) != 0,
                _ => {}
            }
        }

        devices.push(dev);
    }

    if devices.is_empty() {
        return Err(ScrubError::StatusNoDevices {
            fsuuid: expected_fsuuid.to_string(),
        });
    }

    Ok(ScrubStatusRecord {
        fsuuid: expected_fsuuid.to_string(),
        devices,
    })
}

/// Directory holding the btrfs status records (overridable for tests).
fn btrfs_status_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("DAS_BTRFS_STATUS_DIR").unwrap_or_else(|_| BTRFS_STATUS_DIR.to_string()),
    )
}

/// Path of the status record for a filesystem UUID.
///
/// UUID in, path out — there is deliberately no variant of this that takes a
/// mount point.
pub fn status_record_path(fsuuid: &str) -> PathBuf {
    btrfs_status_dir().join(format!("scrub.status.{fsuuid}"))
}

/// Read and parse the persistent status record for a filesystem UUID.
pub fn read_scrub_status(fsuuid: &str) -> Result<ScrubStatusRecord, ScrubError> {
    let path = status_record_path(fsuuid);
    if !path.exists() {
        return Err(ScrubError::StatusMissing {
            path: path.display().to_string(),
        });
    }
    let content = fs::read_to_string(&path).map_err(|e| ScrubError::StatusIo {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    parse_scrub_status(&content, fsuuid)
}

// ---------------------------------------------------------------------------
// Locking
// ---------------------------------------------------------------------------

/// An advisory `flock(2)` held for the lifetime of the value.
///
/// The lock is released when the file descriptor closes on drop; an explicit
/// `LOCK_UN` is issued first so the release is visible even if the `File` is
/// leaked into a longer-lived structure.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
    file: File,
}

impl FileLock {
    fn open(path: &Path) -> Result<File, ScrubError> {
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent).map_err(|e| ScrubError::Lock {
                path: path.display().to_string(),
                detail: format!("cannot create lock directory: {e}"),
            })?;
        }
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o644)
            .open(path)
            .map_err(|e| {
                // `/run` is root-owned, so a permission error here almost
                // always means the caller simply is not root. Say that,
                // instead of surfacing a bare EACCES.
                let denied = matches!(
                    e.raw_os_error(),
                    Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::EROFS)
                );
                // SAFETY: geteuid() is always safe.
                let unprivileged = unsafe { libc::geteuid() } != 0;
                let detail = if denied && unprivileged {
                    format!(
                        "cannot open lock file ({e}) — scrub must run as root \
                         (try: sudo btrdasd scrub run)"
                    )
                } else {
                    format!("cannot open lock file: {e}")
                };
                ScrubError::Lock {
                    path: path.display().to_string(),
                    detail,
                }
            })
    }

    /// `Ok(false)` means, and only means, that another process holds the lock
    /// (`EWOULDBLOCK` from a `LOCK_NB` request). A signal-interrupted call is
    /// retried rather than reported as contention: "skipped" is the one pass
    /// outcome that produces no state file and no email, so it must never be
    /// reached by accident.
    fn flock(file: &File, path: &Path, operation: libc::c_int) -> Result<bool, ScrubError> {
        loop {
            // SAFETY: `file` owns a valid fd for the duration of the call.
            let rc = unsafe { libc::flock(file.as_raw_fd(), operation) };
            if rc == 0 {
                return Ok(true);
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error().unwrap_or(0) {
                libc::EWOULDBLOCK => return Ok(false),
                libc::EINTR => continue,
                _ => {
                    return Err(ScrubError::Lock {
                        path: path.display().to_string(),
                        detail: format!("flock failed: {err}"),
                    });
                }
            }
        }
    }

    /// Try to take the lock without waiting. `Ok(None)` means another process
    /// holds it.
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Option<FileLock>, ScrubError> {
        let path = path.as_ref();
        let file = Self::open(path)?;
        if Self::flock(&file, path, libc::LOCK_EX | libc::LOCK_NB)? {
            Ok(Some(FileLock {
                path: path.to_path_buf(),
                file,
            }))
        } else {
            Ok(None)
        }
    }

    /// Take the lock, waiting as long as necessary.
    ///
    /// A short non-blocking probe runs first so that ordinary sub-second
    /// contention stays quiet; once the wait passes
    /// [`LOCK_WAIT_ANNOUNCE_SECS`] a visible line is logged and the call falls
    /// through to a genuinely blocking `flock`.
    pub fn acquire_blocking(
        path: impl AsRef<Path>,
        progress: &dyn ProgressCallback,
        waiting_message: &str,
    ) -> Result<FileLock, ScrubError> {
        let path = path.as_ref();
        if let Some(lock) = Self::try_acquire(path)? {
            return Ok(lock);
        }
        std::thread::sleep(Duration::from_secs(LOCK_WAIT_ANNOUNCE_SECS));
        if let Some(lock) = Self::try_acquire(path)? {
            return Ok(lock);
        }
        progress.on_log(LogLevel::Info, waiting_message);

        // A blocking LOCK_EX only returns once held (or on a real error);
        // EINTR is retried inside `flock`. A scoped companion thread repeats
        // the notice so a pass parked behind a multi-hour backup is visibly
        // waiting rather than apparently hung.
        let file = Self::open(path)?;
        let done = std::sync::atomic::AtomicBool::new(false);
        let outcome = std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut waited = LOCK_WAIT_ANNOUNCE_SECS;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(1));
                    waited += 1;
                    if waited.is_multiple_of(LOCK_WAIT_REANNOUNCE_SECS)
                        && !done.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        progress.on_log(
                            LogLevel::Info,
                            &format!("{waiting_message} — still waiting after {}m", waited / 60),
                        );
                    }
                }
            });
            let result = Self::flock(&file, path, libc::LOCK_EX);
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            result
        });
        outcome?;
        Ok(FileLock {
            path: path.to_path_buf(),
            file,
        })
    }

    /// Path of the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the fd is valid until `self.file` is dropped, immediately after.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Both locks a scrub pass holds.
///
/// Field order matters: Rust drops fields in declaration order, so
/// `maintenance` is released before `singleton` — the exact reverse of the
/// singleton → maintenance acquisition order used on both this side and
/// `backup-run.sh`'s, which is what makes the pair deadlock-free.
#[derive(Debug)]
pub struct ScrubLocks {
    maintenance: FileLock,
    singleton: FileLock,
}

impl ScrubLocks {
    /// Paths of the held locks, in acquisition order.
    pub fn paths(&self) -> (&Path, &Path) {
        (self.singleton.path(), self.maintenance.path())
    }
}

/// Acquire the singleton lock (non-blocking) then the maintenance lock
/// (blocking). `Ok(None)` means another scrub pass is already running and this
/// invocation must skip.
fn acquire_locks_at(
    singleton_path: &Path,
    maintenance_path: &Path,
    progress: &dyn ProgressCallback,
) -> Result<Option<ScrubLocks>, ScrubError> {
    let Some(singleton) = FileLock::try_acquire(singleton_path)? else {
        return Ok(None);
    };
    let maintenance = FileLock::acquire_blocking(
        maintenance_path,
        progress,
        "waiting for DAS maintenance lock (backup in progress?)",
    )?;
    Ok(Some(ScrubLocks {
        maintenance,
        singleton,
    }))
}

/// Acquire the production scrub locks. `Ok(None)` means skip this invocation.
pub fn acquire_locks(progress: &dyn ProgressCallback) -> Result<Option<ScrubLocks>, ScrubError> {
    acquire_locks_at(
        Path::new(SCRUB_LOCK_PATH),
        Path::new(MAINTENANCE_LOCK_PATH),
        progress,
    )
}

// ---------------------------------------------------------------------------
// Persisted state
// ---------------------------------------------------------------------------

/// Summary of the most recent completed pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassSummary {
    pub started_epoch: i64,
    pub finished_epoch: i64,
    pub targets_attempted: usize,
    pub targets_failed: usize,
    pub success: bool,
}

/// Result of the most recent scrub attempt on one filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptState {
    /// `finished`, `canceled`, `aborted`, or `error`.
    pub outcome: String,
    /// Authoritative pass/fail bit for this filesystem.
    pub ok: bool,
    pub started_epoch: i64,
    pub finished_epoch: i64,
    pub duration_secs: u64,
    pub bytes_scrubbed: u64,
    pub error_total: u64,
    pub counters: ScrubCounters,
    #[serde(default)]
    pub messages: Vec<String>,
}

/// Per-filesystem state, keyed in [`ScrubState::filesystems`] by FS UUID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsState {
    pub target_label: String,
    pub mountpoint: String,
    /// Epoch of the last *successful* scrub, carried forward across failures.
    /// This is the field to age against `warn_age_days` / `fail_age_days`.
    #[serde(default)]
    pub last_success_epoch: Option<i64>,
    pub last_attempt: AttemptState,
}

/// The whole state file. See the module docs for the schema contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrubState {
    pub schema_version: u32,
    #[serde(default)]
    pub last_pass: Option<PassSummary>,
    #[serde(default)]
    pub filesystems: BTreeMap<String, FsState>,
}

impl Default for ScrubState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            last_pass: None,
            filesystems: BTreeMap::new(),
        }
    }
}

/// Path of the state file (overridable for tests via `DAS_SCRUB_STATE`).
pub fn state_path() -> PathBuf {
    PathBuf::from(std::env::var("DAS_SCRUB_STATE").unwrap_or_else(|_| SCRUB_STATE_PATH.to_string()))
}

/// Load the state file. A missing file yields the default (empty) state.
///
/// **Pure**: this never writes, renames, or deletes anything, so the
/// unprivileged readers (health checks, the GUI) can call it freely and get an
/// error that means exactly one thing. Unparseable JSON is reported as
/// [`ScrubError::StateCorrupt`]; every other failure (EIO, EACCES, a file that
/// is intact but momentarily unreadable) is [`ScrubError::State`] and the file
/// must not be overwritten — doing so would destroy the `last_success_epoch`
/// history silently. Quarantine is the writer's job:
/// [`quarantine_state_file`].
pub fn load_state_from(path: &Path) -> Result<ScrubState, ScrubError> {
    if !path.exists() {
        return Ok(ScrubState::default());
    }
    let content = fs::read_to_string(path).map_err(|e| ScrubError::State {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    serde_json::from_str::<ScrubState>(&content).map_err(|e| ScrubError::StateCorrupt {
        path: path.display().to_string(),
        detail: e.to_string(),
    })
}

/// Move an unparseable state file aside to `<path>.corrupt`, preserving the
/// evidence while freeing the canonical path for a fresh write.
///
/// Root-only in practice (the state file lives under `/var/lib/das-backup`).
/// Callers must have established that the file is corrupt — never call this
/// for a file that merely could not be read.
pub fn quarantine_state_file(path: &Path) -> Result<PathBuf, ScrubError> {
    // Append rather than `with_extension`, so `scrub-state.json` becomes
    // `scrub-state.json.corrupt` and cannot collide with the live file's stem.
    let corrupt = PathBuf::from(format!("{}.corrupt", path.display()));
    fs::rename(path, &corrupt).map_err(|e| ScrubError::State {
        path: path.display().to_string(),
        detail: format!(
            "cannot quarantine corrupt state file to {}: {e}",
            corrupt.display()
        ),
    })?;
    Ok(corrupt)
}

/// Load the state file from the configured location.
pub fn load_state() -> Result<ScrubState, ScrubError> {
    load_state_from(&state_path())
}

/// Write the state file atomically (temp file + rename), mode `0644`.
pub fn save_state_to(state: &ScrubState, path: &Path) -> Result<(), ScrubError> {
    let err = |detail: String| ScrubError::State {
        path: path.display().to_string(),
        detail,
    };
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent).map_err(|e| err(format!("cannot create directory: {e}")))?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| err(format!("cannot serialize state: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = File::create(&tmp).map_err(|e| err(format!("cannot create temp file: {e}")))?;
        f.write_all(json.as_bytes())
            .map_err(|e| err(format!("cannot write temp file: {e}")))?;
        f.write_all(b"\n")
            .map_err(|e| err(format!("cannot write temp file: {e}")))?;
        f.sync_all()
            .map_err(|e| err(format!("cannot fsync: {e}")))?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))
        .map_err(|e| err(format!("cannot chmod temp file: {e}")))?;
    fs::rename(&tmp, path).map_err(|e| err(format!("cannot rename temp file: {e}")))?;
    // fsync the directory so the rename itself survives a crash — without it
    // the file contents are durable but the name may not be, and health checks
    // would read a state file that no longer exists.
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Write the state file to the configured location.
pub fn save_state(state: &ScrubState) -> Result<(), ScrubError> {
    save_state_to(state, &state_path())
}

/// Fold a completed pass into an existing state, preserving `last_success_epoch`
/// for filesystems whose latest attempt failed.
pub fn merge_pass_into_state(state: &mut ScrubState, pass: &ScrubPass) {
    state.schema_version = STATE_SCHEMA_VERSION;
    for result in &pass.results {
        if result.fsuuid.is_empty() {
            // No UUID resolved — there is no stable key to file this under, so
            // it stays in the email/report only.
            continue;
        }
        let ok = result.ok();
        let prior_success = state
            .filesystems
            .get(&result.fsuuid)
            .and_then(|f| f.last_success_epoch);
        let last_success_epoch = if ok {
            Some(if result.finished_epoch > 0 {
                result.finished_epoch
            } else {
                pass.finished_epoch
            })
        } else {
            prior_success
        };
        let counters = result.counters;
        state.filesystems.insert(
            result.fsuuid.clone(),
            FsState {
                target_label: result.target_label.clone(),
                mountpoint: result.mountpoint.clone(),
                last_success_epoch,
                last_attempt: AttemptState {
                    outcome: result
                        .outcome
                        .map(|o| o.as_str().to_string())
                        .unwrap_or_else(|| "error".to_string()),
                    ok,
                    started_epoch: result.started_epoch,
                    finished_epoch: result.finished_epoch,
                    duration_secs: result.duration_secs,
                    bytes_scrubbed: result.bytes_scrubbed,
                    error_total: counters.error_total(),
                    counters,
                    messages: result.errors.clone(),
                },
            },
        );
    }
    state.last_pass = Some(PassSummary {
        started_epoch: pass.started_epoch,
        finished_epoch: pass.finished_epoch,
        targets_attempted: pass.results.len(),
        targets_failed: pass.failed_count(),
        success: pass.success(),
    });
}

// ---------------------------------------------------------------------------
// Pass results
// ---------------------------------------------------------------------------

/// Whether the pass ran or stepped aside for another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassStatus {
    /// The pass ran (individual filesystems may still have failed).
    Completed,
    /// Another scrub held the singleton lock — nothing was scrubbed, and no
    /// state was written.
    Skipped,
}

/// Outcome of scrubbing one filesystem.
#[derive(Debug, Clone)]
pub struct ScrubFsResult {
    pub target_label: String,
    /// Empty only when the UUID could not be resolved at all.
    pub fsuuid: String,
    pub mountpoint: String,
    /// `None` when no usable status record was produced.
    pub outcome: Option<ScrubOutcome>,
    pub counters: ScrubCounters,
    pub bytes_scrubbed: u64,
    pub started_epoch: i64,
    pub finished_epoch: i64,
    pub duration_secs: u64,
    /// Engine-level problems (mount, unmount, stale record, …). Non-empty
    /// means failure regardless of the outcome field.
    pub errors: Vec<String>,
    /// Whether this engine mounted the filesystem (and therefore unmounted it).
    pub mounted_by_engine: bool,
    /// Whether a real `btrfs scrub start` child process was confirmed to
    /// launch for this filesystem — `true` only once `Command::spawn()` has
    /// actually succeeded, never set pre-emptively.
    ///
    /// This is the authoritative "was a scrub genuinely attempted" signal
    /// consumed by the CLI's `exit_code_for_pass` (`bd
    /// DAS-Backup-Manager-18p`): if `spawn()` fails for every configured
    /// target (binary missing, broken `PATH`, exec format error — a fast,
    /// systemic setup failure), no scrub was ever issued anywhere, and that
    /// must be distinguishable from "the pass ran but found damage" so
    /// Sentinel's fast-retry path still engages for it. `started_epoch` is
    /// deliberately *not* used for this purpose: it used to be stamped
    /// before the spawn was attempted, which meant a spawn failure still
    /// left it non-zero and masqueraded as "ran". This field is set exactly
    /// once, in lockstep with the first moment `started_epoch` is stamped.
    pub scrub_launched: bool,
}

impl ScrubFsResult {
    fn new(target_label: &str) -> Self {
        Self {
            target_label: target_label.to_string(),
            fsuuid: String::new(),
            mountpoint: String::new(),
            outcome: None,
            counters: ScrubCounters::default(),
            bytes_scrubbed: 0,
            started_epoch: 0,
            finished_epoch: 0,
            duration_secs: 0,
            errors: Vec::new(),
            mounted_by_engine: false,
            scrub_launched: false,
        }
    }

    /// A filesystem passed only if it finished, has no damage counters, and
    /// the engine hit no problems around it.
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
            && self.outcome == Some(ScrubOutcome::Finished)
            && !self.counters.has_errors()
    }

    /// One-line status word for reports.
    pub fn status_word(&self) -> &'static str {
        if self.ok() {
            "OK"
        } else if !self.errors.is_empty() {
            // Engine-level problems (mount, unmount, stale record) outrank
            // whatever the status record happens to say.
            "FAILED"
        } else {
            match self.outcome {
                Some(ScrubOutcome::Aborted) => "ABORTED",
                Some(ScrubOutcome::Canceled) => "CANCELED",
                Some(ScrubOutcome::Finished) => "ERRORS",
                None => "FAILED",
            }
        }
    }
}

/// Result of a whole scrub pass.
#[derive(Debug, Clone)]
pub struct ScrubPass {
    pub status: PassStatus,
    pub started_epoch: i64,
    pub finished_epoch: i64,
    pub results: Vec<ScrubFsResult>,
    /// Pass-level problems that are not attributable to one filesystem.
    pub errors: Vec<String>,
}

impl ScrubPass {
    /// Number of filesystems that did not pass.
    pub fn failed_count(&self) -> usize {
        self.results.iter().filter(|r| !r.ok()).count()
    }

    /// Whether every scrubbed filesystem passed and the pass hit no
    /// pass-level errors. A skipped pass counts as a success (nothing broke).
    pub fn success(&self) -> bool {
        self.errors.is_empty() && self.failed_count() == 0
    }
}

// ---------------------------------------------------------------------------
// UUID resolution
// ---------------------------------------------------------------------------

/// Resolve a config target label to its BTRFS filesystem UUID.
///
/// Prefers the target's explicit `mount_uuid`. Targets that predate that field
/// are resolved through their drive serial to a partition device and then
/// `blkid`. Never derived from a mount point — a mount point can be backed by
/// the wrong filesystem, or by no filesystem at all.
pub fn resolve_target_fsuuid(config: &Config, label: &str) -> Result<String, String> {
    let target = config
        .targets
        .iter()
        .find(|t| t.label == label)
        .ok_or_else(|| format!("no [[target]] with label '{label}' in config"))?;

    if let Some(uuid) = &target.mount_uuid
        && !uuid.is_empty()
    {
        return Ok(uuid.clone());
    }

    let serials = target.effective_serials();
    if serials.is_empty() {
        return Err(format!(
            "target '{label}' has neither mount_uuid nor any serial — cannot resolve filesystem UUID"
        ));
    }

    let mut tried = Vec::new();
    for serial in &serials {
        let Some(dev) = crate::health::device_from_serial(serial) else {
            tried.push(format!("{serial}: drive not present"));
            continue;
        };
        let part = partition_device(&dev, &target.role);
        match blkid_uuid(&part) {
            Some(uuid) => return Ok(uuid),
            None => tried.push(format!("{serial}: blkid found no UUID on {part}")),
        }
    }

    Err(format!(
        "target '{label}': could not resolve filesystem UUID ({})",
        tried.join("; ")
    ))
}

/// Filesystem UUID of a block device, via `blkid`.
fn blkid_uuid(device: &str) -> Option<String> {
    let out = Command::new("blkid")
        .args(["-s", "UUID", "-o", "value", device])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uuid.is_empty() { None } else { Some(uuid) }
}

/// Whether a scrub is running *right now* on a mounted filesystem, as the
/// kernel sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveScrubState {
    /// The kernel reports an in-progress scrub.
    Running,
    /// The kernel reports no scrub in progress.
    NotRunning,
    /// Could not be determined — treated as "assume something is running", so
    /// the engine never forces on a guess.
    Unknown,
}

/// Ask `btrfs scrub status` whether a scrub is live on this filesystem.
///
/// This is the one place a mount *path* is used for a scrub query, so the
/// `expected_fsuuid` parameter is not optional decoration — it is what makes
/// the call safe. `btrfs scrub status <path>` on a path that is *not*
/// currently a mountpoint for that exact filesystem silently answers for
/// whatever backing filesystem the path happens to resolve through instead
/// (an idle/unmounted DAS target's configured mountpoint is a perfectly
/// ordinary directory that falls through to its parent filesystem — see the
/// 2026-08-01 `bd DAS-Backup-Manager-0kn` review: a caller that skipped this
/// verification could observe an unrelated system `btrfs-scrub@` pass
/// running on the backing filesystem and mistake it for *this* filesystem's
/// scrub). This is exactly the same hazard class `parse_scrub_status`'s UUID
/// check guards against for the persisted record — the fix here is the same
/// shape: verify by UUID, via `findmnt`, before trusting anything read
/// through a path.
///
/// Returns [`LiveScrubState::Unknown`] whenever `mount_point` is not
/// currently backed by `expected_fsuuid` (including "nothing is mounted
/// there at all") — every caller in this codebase already treats `Unknown`
/// exactly like `NotRunning` for any action gated on `Running`, so an
/// unverified mount safely resolves to "do nothing", never to a false
/// positive. When the verification passes, result interpretation is kernel
/// truth: `Status: running` is only printed while a scrub is actually in
/// progress (verified empirically, 2026-08-01 loopback rig).
pub fn live_scrub_state(mount_point: &str, expected_fsuuid: &str) -> LiveScrubState {
    match mounted_fs_uuid(mount_point) {
        Some(found) if found.eq_ignore_ascii_case(expected_fsuuid) => {}
        _ => return LiveScrubState::Unknown,
    }
    let Ok(out) = Command::new("btrfs")
        .args(["scrub", "status", mount_point])
        .output()
    else {
        return LiveScrubState::Unknown;
    };
    if !out.status.success() {
        return LiveScrubState::Unknown;
    }
    parse_scrub_status_output(&String::from_utf8_lossy(&out.stdout))
}

/// Extract the liveness verdict from `btrfs scrub status` output.
///
/// btrfs-progs v7.1 prints exactly one `Status:` line per filesystem:
/// `running`, `finished`, `aborted` (a cancelled scrub), or `interrupted` (the
/// aborted-record shape). Output with no `Status:` line at all — which happens
/// when a just-started scrub has no stats yet — is [`LiveScrubState::Unknown`],
/// never "not running", because that case *is* a live scrub.
pub fn parse_scrub_status_output(stdout: &str) -> LiveScrubState {
    for line in stdout.lines() {
        if let Some(value) = line.trim().strip_prefix("Status:") {
            return match value.trim() {
                "running" => LiveScrubState::Running,
                "" => LiveScrubState::Unknown,
                _ => LiveScrubState::NotRunning,
            };
        }
    }
    LiveScrubState::Unknown
}

/// Whether the next `btrfs scrub start` needs `-f`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrubStartMode {
    /// Plain `btrfs scrub start -B`.
    Normal,
    /// `btrfs scrub start -B -f`, because a previous pass left an aborted
    /// record behind and nothing is running now.
    Forced { reason: String },
}

/// Decide whether this filesystem's scrub must be forced.
///
/// `btrfs-progs` decides "a scrub is already running" partly from the saved
/// status record: a device row that is neither `finished` nor `canceled` can
/// read as a live scrub even when nothing is running, and the documented
/// remedy is `-f` ("useful when scrub status file is damaged and reports a
/// running scrub although it is not", `man btrfs-scrub`). Left unhandled, an
/// aborted pass can wedge every later scrub of that filesystem until a human
/// intervenes.
///
/// `-f` is emphatically **not** unconditional: the locks in this module do not
/// protect against an operator running `btrfs scrub start` by hand, and
/// force-restarting somebody else's scrub would throw away hours of work. It is
/// used only when the prior record is [`ScrubOutcome::Aborted`] **and** the
/// kernel confirms nothing is running. Anything unknown falls back to a plain
/// start, which fails loudly rather than stomping.
pub fn decide_scrub_start_mode(fsuuid: &str, mount_point: &str) -> ScrubStartMode {
    let Ok(prior) = read_scrub_status(fsuuid) else {
        // No prior record at all — nothing to clear.
        return ScrubStartMode::Normal;
    };
    if prior.outcome() != ScrubOutcome::Aborted {
        return ScrubStartMode::Normal;
    }
    match live_scrub_state(mount_point, fsuuid) {
        LiveScrubState::NotRunning => ScrubStartMode::Forced {
            reason: format!(
                "previous scrub of {fsuuid} left an aborted record \
                 (canceled:0 finished:0) and the kernel reports no scrub running — \
                 forcing a fresh scrub so the stale record cannot block it"
            ),
        },
        LiveScrubState::Running => ScrubStartMode::Normal,
        LiveScrubState::Unknown => ScrubStartMode::Normal,
    }
}

/// Filesystem UUID currently mounted at `mount_point`, via `findmnt`.
///
/// Returns `None` when nothing is mounted there. Used only to *verify* an
/// expected UUID after mounting — never to discover which filesystem to act on.
fn mounted_fs_uuid(mount_point: &str) -> Option<String> {
    let out = Command::new("findmnt")
        .args(["-n", "-o", "UUID", "--mountpoint", mount_point])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uuid.is_empty() { None } else { Some(uuid) }
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/// Current unix time.
fn now_epoch() -> i64 {
    let mut t: libc::time_t = 0;
    // SAFETY: `libc::time` writes at most one `time_t` through the pointer.
    unsafe { libc::time(&mut t) };
    t as i64
}

/// Run the full scrub pass: locks, then every configured target in sequence.
///
/// This deliberately does **not** consult `config.scrub.enabled` — a manual
/// `btrdasd scrub run` must work even when the schedule is switched off. The
/// scheduled path gates on `enabled` when installing/enabling the timer.
///
/// Returns `Ok` with [`PassStatus::Skipped`] when another scrub already holds
/// the singleton lock; that is a success, not a failure, and writes no state.
///
/// [`ScrubPass::success()`] is the library's own "did everything pass
/// cleanly" bit — the source of truth for the FAILURE email and
/// `btrdasd health` Critical escalation, and it is **not** the same
/// question as "should the calling process exit nonzero". The CLI
/// (`indexer/src/main.rs`, `exit_code_for_pass`) deliberately maps a pass
/// that ran but found errors to exit code 0: `btrdasd scrub run` runs under
/// a Sentinel-monitored systemd unit whose restart-rate limiter cannot
/// brake failures spaced a month apart, so treating "ran with damage found"
/// the same as "could not run at all" would retry-loop a real multi-hour
/// scrub over failing hardware indefinitely (`bd DAS-Backup-Manager-18p`).
/// This function's return value stays truthful either way — only the CLI's
/// interpretation of it for the process exit code is split.
pub fn run_scrub_pass(
    config: &Config,
    progress: &dyn ProgressCallback,
) -> Result<ScrubPass, ScrubError> {
    if config.scrub.targets.is_empty() {
        return Err(ScrubError::NoTargets);
    }

    let started_epoch = now_epoch();

    let Some(_locks) = acquire_locks(progress)? else {
        progress.on_log(
            LogLevel::Info,
            &format!("Another scrub holds {SCRUB_LOCK_PATH} — skipping this invocation"),
        );
        // Close the progress lifecycle on this path too, so a caller driving a
        // UI or a log does not see a stage opened and never completed.
        progress.on_complete(true, "Skipped — another scrub pass is already running");
        return Ok(ScrubPass {
            status: PassStatus::Skipped,
            started_epoch,
            finished_epoch: now_epoch(),
            results: Vec::new(),
            errors: Vec::new(),
        });
    };

    if !config.scrub.enabled {
        progress.on_log(
            LogLevel::Warning,
            "[scrub].enabled is false in config — running anyway (manual/forced invocation)",
        );
    }

    let total = config.scrub.targets.len() as u64;
    progress.on_stage("Scrubbing DAS filesystems", total);

    let mut results = Vec::new();
    // Sequential by construction: the three DAS filesystems share one USB bus.
    for (i, label) in config.scrub.targets.iter().enumerate() {
        progress.on_progress((i + 1) as u64, total, &format!("Scrubbing {label}"));
        let result = scrub_one_target(config, label, progress);
        if result.ok() {
            progress.on_log(
                LogLevel::Info,
                &format!(
                    "{label}: scrub finished cleanly ({} in {}s)",
                    format_bytes(result.bytes_scrubbed),
                    result.duration_secs
                ),
            );
        } else {
            progress.on_log(
                LogLevel::Error,
                &format!("{label}: scrub FAILED — {}", describe_failure(&result)),
            );
        }
        results.push(result);
    }

    let mut pass = ScrubPass {
        status: PassStatus::Completed,
        started_epoch,
        finished_epoch: now_epoch(),
        results,
        errors: Vec::new(),
    };

    // Persist before emailing: the state file is what health checks read, and
    // it must survive an email failure.
    match persist_pass(&pass, progress) {
        Ok(path) => progress.on_log(
            LogLevel::Info,
            &format!("Scrub state written to {}", path.display()),
        ),
        Err(e) => {
            let msg = format!("Failed to persist scrub state: {e}");
            progress.on_log(LogLevel::Error, &msg);
            pass.errors.push(msg);
        }
    }

    let report = format_scrub_report(&pass, config);
    match crate::report::send_email_report_with_kind(&report, config, "Scrub") {
        Ok(()) => progress.on_log(LogLevel::Info, "Scrub report emailed"),
        Err(e) => progress.on_log(
            LogLevel::Warning,
            &format!("Could not send scrub report email: {e}"),
        ),
    }

    progress.on_complete(
        pass.success(),
        &format!(
            "{} of {} filesystems scrubbed cleanly",
            pass.results.len() - pass.failed_count(),
            pass.results.len()
        ),
    );

    Ok(pass)
}

/// Merge a pass into the persisted state and write it back.
///
/// A corrupt state file must not cost us this pass's result: it is quarantined
/// here (the writer's job, not the reader's — see [`load_state_from`]) and a
/// fresh state is written in its place. Any *other* read failure aborts the
/// write: the existing file may be intact and holds the `last_success_epoch`
/// history that health checks age against, and overwriting it from a blank
/// state would erase that silently.
fn persist_pass(pass: &ScrubPass, progress: &dyn ProgressCallback) -> Result<PathBuf, ScrubError> {
    let path = state_path();
    let mut state = match load_state_from(&path) {
        Ok(state) => state,
        Err(ScrubError::StateCorrupt { detail, .. }) => {
            let moved = quarantine_state_file(&path)?;
            progress.on_log(
                LogLevel::Warning,
                &format!(
                    "Scrub state file was unparseable ({detail}) — quarantined to {} and rebuilt",
                    moved.display()
                ),
            );
            ScrubState::default()
        }
        Err(e) => return Err(e),
    };
    merge_pass_into_state(&mut state, pass);
    save_state_to(&state, &path)?;
    Ok(path)
}

/// Short human description of why a filesystem's scrub did not pass.
fn describe_failure(result: &ScrubFsResult) -> String {
    if !result.errors.is_empty() {
        return result.errors.join("; ");
    }
    match result.outcome {
        Some(ScrubOutcome::Aborted) => {
            format!(
                "scrub aborted (did not complete); {}",
                result.counters.summary()
            )
        }
        Some(ScrubOutcome::Canceled) => {
            format!("scrub canceled; {}", result.counters.summary())
        }
        Some(ScrubOutcome::Finished) => {
            format!("errors found: {}", result.counters.summary())
        }
        None => "no scrub status record produced".to_string(),
    }
}

/// Mount, scrub, parse, unmount — one filesystem.
fn scrub_one_target(
    config: &Config,
    label: &str,
    progress: &dyn ProgressCallback,
) -> ScrubFsResult {
    let mut result = ScrubFsResult::new(label);

    let fsuuid = match resolve_target_fsuuid(config, label) {
        Ok(u) => u,
        Err(e) => {
            result.errors.push(e);
            return result;
        }
    };
    result.fsuuid = fsuuid.clone();

    let Some(target) = config.targets.iter().find(|t| t.label == label) else {
        result
            .errors
            .push(format!("no [[target]] with label '{label}' in config"));
        return result;
    };
    result.mountpoint = target.mount.clone();
    let mount_point = target.mount.clone();

    // --- mount ------------------------------------------------------------
    match ensure_mounted(&mount_point, &fsuuid, &config.das.mount_opts, progress) {
        Ok(mounted_by_engine) => result.mounted_by_engine = mounted_by_engine,
        Err(e) => {
            result.errors.push(e);
            return result;
        }
    }

    // --- scrub ------------------------------------------------------------
    // `scrub_started` anchors the freshness check below regardless of
    // outcome, but `result.started_epoch`/`result.scrub_launched` are
    // deliberately only stamped once `run_btrfs_scrub` confirms the child
    // process actually launched — see the doc comment on
    // `ScrubFsResult::scrub_launched` for why a pre-emptive stamp here was a
    // bug (`bd DAS-Backup-Manager-18p` review).
    let scrub_started = now_epoch();
    match run_btrfs_scrub(&mount_point, &fsuuid, progress) {
        ScrubRunOutcome::SpawnFailed(e) => {
            result.errors.push(e);
        }
        ScrubRunOutcome::Launched(outcome) => {
            result.started_epoch = scrub_started;
            result.scrub_launched = true;
            if let Err(e) = outcome {
                result.errors.push(e);
            }
        }
    }

    // --- status by UUID (never by path) -----------------------------------
    match read_scrub_status(&fsuuid) {
        Ok(record) => {
            // A record left over from an earlier scrub must never be read as
            // this pass's result — the 2026-07-27 stale-record trap. The
            // anchor is *this filesystem's* start, not the pass's: a pass runs
            // for many hours, so anchoring on the pass would accept a record
            // written half a day before this target was even touched.
            let activity = record.last_activity_start();
            if activity == 0 {
                result.errors.push(format!(
                    "scrub status record for {fsuuid} has no start timestamp — cannot confirm it \
                     belongs to this run"
                ));
            } else if activity < scrub_started - RECORD_FRESHNESS_SLACK_SECS {
                result.errors.push(format!(
                    "scrub status record for {fsuuid} is stale (last start/resume {activity} \
                     predates this filesystem's scrub start {scrub_started}) — no result was \
                     written for this run"
                ));
            } else {
                let t_start = record.t_start();
                result.outcome = Some(record.outcome());
                result.counters = record.counters();
                result.bytes_scrubbed = record.bytes_scrubbed();
                result.duration_secs = record.duration_secs();
                result.started_epoch = t_start;
                result.finished_epoch = record.finished_epoch();
            }
        }
        Err(e) => result.errors.push(e.to_string()),
    }

    if result.finished_epoch == 0 {
        result.finished_epoch = now_epoch();
    }

    // --- unmount ----------------------------------------------------------
    if result.mounted_by_engine {
        if let Err(e) = unmount(&mount_point, progress) {
            // Reportable, never silent: a target left mounted blocks the next
            // backup's mount hygiene checks.
            result.errors.push(e);
        }
    } else {
        progress.on_log(
            LogLevel::Info,
            &format!(
                "{label}: {mount_point} was already mounted before this pass — leaving it mounted"
            ),
        );
    }

    result
}

/// Ensure `mount_point` holds the filesystem with `fsuuid`.
///
/// Returns whether this call performed the mount (and therefore owns the
/// unmount). Any filesystem found at the mount point that is *not* the expected
/// one is an error — never an invitation to unmount it.
fn ensure_mounted(
    mount_point: &str,
    fsuuid: &str,
    mount_opts: &str,
    progress: &dyn ProgressCallback,
) -> Result<bool, String> {
    let path = Path::new(mount_point);

    if path.exists() && crate::health::is_mountpoint(path) {
        return match mounted_fs_uuid(mount_point) {
            Some(found) if found.eq_ignore_ascii_case(fsuuid) => {
                progress.on_log(
                    LogLevel::Info,
                    &format!("{mount_point} already holds {fsuuid} — reusing existing mount"),
                );
                Ok(false)
            }
            Some(found) => Err(format!(
                "{mount_point} is mounted but holds filesystem '{found}', expected '{fsuuid}' — \
                 refusing to scrub the wrong filesystem"
            )),
            None => Err(format!(
                "{mount_point} is a mountpoint but findmnt could not report its filesystem UUID"
            )),
        };
    }

    // Track whether the directory is ours, so a failed mount can take it back
    // out again. A bare directory left at a configured target path is how
    // btrbk once wrote a backup onto the root filesystem (bd
    // DAS-Backup-Manager-9on) — the engine must not manufacture one.
    let created_dir = !path.exists();
    if created_dir {
        fs::create_dir_all(path)
            .map_err(|e| format!("cannot create mount point {mount_point}: {e}"))?;
    }
    let cleanup_dir = |progress: &dyn ProgressCallback| {
        if created_dir && fs::remove_dir(path).is_ok() {
            progress.on_log(
                LogLevel::Info,
                &format!("Removed mount point {mount_point} created for this pass"),
            );
        }
    };

    let mut cmd = Command::new("mount");
    cmd.args(["-t", "btrfs"]);
    if !mount_opts.is_empty() {
        cmd.args(["-o", mount_opts]);
    }
    cmd.arg(format!("UUID={fsuuid}")).arg(mount_point);

    let out = match cmd.output() {
        Ok(out) => out,
        Err(e) => {
            cleanup_dir(progress);
            return Err(format!("cannot execute mount for UUID={fsuuid}: {e}"));
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        cleanup_dir(progress);
        return Err(format!(
            "mount UUID={fsuuid} → {mount_point} failed: {stderr}"
        ));
    }

    // Verify what actually landed there. A successful mount of the wrong
    // filesystem is the exact class of bug this module exists to prevent.
    match mounted_fs_uuid(mount_point) {
        Some(found) if found.eq_ignore_ascii_case(fsuuid) => {
            progress.on_log(
                LogLevel::Info,
                &format!("Mounted {fsuuid} at {mount_point}"),
            );
            Ok(true)
        }
        other => {
            let _ = Command::new("umount").arg(mount_point).status();
            cleanup_dir(progress);
            Err(format!(
                "post-mount verification failed for {mount_point}: expected UUID '{fsuuid}', \
                 found '{}' — unmounted again",
                other.unwrap_or_else(|| "<none>".to_string())
            ))
        }
    }
}

/// Unmount, retrying once — USB targets occasionally report EBUSY while the
/// kernel finishes writeback.
fn unmount(mount_point: &str, progress: &dyn ProgressCallback) -> Result<(), String> {
    for attempt in 1..=2 {
        let out = Command::new("umount")
            .arg(mount_point)
            .output()
            .map_err(|e| format!("cannot execute umount {mount_point}: {e}"))?;
        if out.status.success() {
            progress.on_log(LogLevel::Info, &format!("Unmounted {mount_point}"));
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if attempt == 2 {
            return Err(format!("umount {mount_point} failed: {stderr}"));
        }
        progress.on_log(
            LogLevel::Warning,
            &format!("umount {mount_point} failed ({stderr}) — retrying in 5s"),
        );
        std::thread::sleep(Duration::from_secs(5));
    }
    unreachable!("loop returns on both attempts")
}

/// Outcome of attempting to run `btrfs scrub start` against one filesystem.
///
/// The distinction that matters is whether the child process ever launched
/// at all. `Command::spawn()` failing (binary missing, broken `PATH`, exec
/// format error) means **no scrub was ever issued** — a fast, systemic setup
/// failure, indistinguishable in kind from a config load error. Everything
/// that can go wrong *after* a successful spawn (nonzero exit, a failed
/// `wait()`) means a real scrub attempt genuinely happened, even though it
/// didn't succeed. Conflating the two was the exact bug flagged in the `bd
/// DAS-Backup-Manager-18p` review: stamping `started_epoch` before spawn was
/// attempted meant a spawn failure on every target still looked like "the
/// pass ran" to `exit_code_for_pass`, losing the fast-retry signal Sentinel
/// needs for a genuine can't-even-start failure.
#[derive(Debug)]
enum ScrubRunOutcome {
    /// The child process never launched. No scrub was issued.
    SpawnFailed(String),
    /// The child process launched — a real `btrfs scrub start` ran — with
    /// `Ok(())` if it exited zero, `Err` describing what went wrong
    /// otherwise (nonzero exit, or the wait itself failing).
    Launched(Result<(), String>),
}

/// Run `btrfs scrub start -B` in the foreground, logging progress read from the
/// UUID-keyed status record while it runs.
///
/// The exit status is logged but never used as the verdict — the status record
/// is the source of truth, because a scrub can exit zero and still have
/// aborted.
fn run_btrfs_scrub(
    mount_point: &str,
    fsuuid: &str,
    progress: &dyn ProgressCallback,
) -> ScrubRunOutcome {
    // Clear a stale aborted record out of the way if — and only if — nothing
    // is running. See `decide_scrub_start_mode`.
    let mut args = vec!["scrub", "start", "-B"];
    match decide_scrub_start_mode(fsuuid, mount_point) {
        ScrubStartMode::Forced { reason } => {
            progress.on_log(
                LogLevel::Warning,
                &format!("Using 'btrfs scrub start -B -f': {reason}"),
            );
            args.push("-f");
        }
        ScrubStartMode::Normal => {}
    }
    args.push(mount_point);

    let mut child = match Command::new("btrfs")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ScrubRunOutcome::SpawnFailed(format!(
                "cannot start btrfs scrub on {mount_point}: {e}"
            ));
        }
    };
    // From here on the child genuinely launched — every path below is
    // `ScrubRunOutcome::Launched`, whatever its inner `Result`.

    // Drain stderr on a dedicated thread. The poll loop below cannot read the
    // pipe itself, and a child blocked writing into a full pipe would never
    // exit — wedging the pass while it holds both DAS locks, with no timeout
    // anywhere to break the deadlock.
    let stderr_reader = child.stderr.take().map(|mut handle| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = handle.read_to_string(&mut buf);
            buf
        })
    });

    let started = Instant::now();
    let mut last_log = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                // Never walk away from a live scrub: the caller unmounts this
                // filesystem next, and an orphan would keep hammering the
                // shared USB bus while the next target scrubs.
                let _ = child.kill();
                let _ = child.wait();
                return ScrubRunOutcome::Launched(Err(format!(
                    "waiting for btrfs scrub failed: {e}"
                )));
            }
        }
        std::thread::sleep(SCRUB_POLL_INTERVAL);
        if last_log.elapsed().as_secs() >= SCRUB_PROGRESS_LOG_SECS {
            last_log = Instant::now();
            // Progress is read by UUID, like everything else.
            if let Ok(record) = read_scrub_status(fsuuid) {
                progress.on_log(
                    LogLevel::Info,
                    &format!(
                        "scrub {fsuuid}: {} scrubbed after {}m",
                        format_bytes(record.bytes_scrubbed()),
                        started.elapsed().as_secs() / 60
                    ),
                );
            }
        }
    };

    let stderr = stderr_reader
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();

    ScrubRunOutcome::Launched(if status.success() {
        Ok(())
    } else {
        Err(format!(
            "btrfs scrub start -B {mount_point} exited {}{}",
            status.code().unwrap_or(-1),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        ))
    })
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Format bytes with binary units (matches the backup report's style).
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Format a duration in seconds as `HHhMMm`.
fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m {}s", secs % 60)
    }
}

/// Render the email body for a pass.
///
/// The word `FAILURE` appears in the header when anything failed — the shared
/// mailer derives the subject's status word from the body.
pub fn format_scrub_report(pass: &ScrubPass, config: &Config) -> String {
    let sep = "═".repeat(63);
    let thin = "─".repeat(63);
    let mut r = String::with_capacity(2048);

    let overall = if pass.status == PassStatus::Skipped {
        "SKIPPED (another scrub was running)"
    } else if pass.success() {
        "ALL SCRUBS COMPLETED CLEANLY"
    } else {
        "FAILURES DETECTED"
    };

    r.push_str(&format!(
        "{sep}\n  DAS Scrub Report\n  Status: {overall}\n{sep}\n\n"
    ));

    let elapsed = (pass.finished_epoch - pass.started_epoch).max(0) as u64;
    r.push_str(&format!("PASS SUMMARY\n{thin}\n"));
    r.push_str(&format!("  Filesystems scrubbed  {}\n", pass.results.len()));
    r.push_str(&format!(
        "  Failures              {}\n",
        pass.failed_count()
    ));
    r.push_str(&format!(
        "  Elapsed               {}\n",
        format_duration(elapsed)
    ));
    r.push_str(&format!(
        "  Warn / fail age       {} / {} days\n",
        config.scrub.warn_age_days, config.scrub.fail_age_days
    ));

    r.push_str(&format!("\nPER-FILESYSTEM RESULTS\n{thin}\n"));
    for result in &pass.results {
        r.push_str(&format!(
            "  {:<24} {:<9} {:<11} {}\n",
            result.target_label,
            result.status_word(),
            format_bytes(result.bytes_scrubbed),
            format_duration(result.duration_secs)
        ));
        r.push_str(&format!(
            "    uuid={} outcome={} errors={}\n",
            if result.fsuuid.is_empty() {
                "<unresolved>"
            } else {
                &result.fsuuid
            },
            result.outcome.map(|o| o.as_str()).unwrap_or("<no record>"),
            result.counters.summary()
        ));
    }

    let mut problems: Vec<String> = pass.errors.clone();
    for result in &pass.results {
        if !result.ok() {
            problems.push(format!(
                "{}: {}",
                result.target_label,
                describe_failure(result)
            ));
        }
    }
    if !problems.is_empty() {
        r.push_str(&format!("\nPROBLEMS\n{thin}\n"));
        for p in &problems {
            r.push_str(&format!("  • {p}\n"));
        }
    }

    r.push_str(&format!("\n{sep}\n"));
    r
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{NullProgress, TestProgress};

    /// Real record read from `/var/lib/btrfs/scrub.status.60b05268-…` on the
    /// author's host (recovery-A, scrubbed 2026-07-27 — the good pass that the
    /// investigation ended with).
    const REAL_FINISHED: &str = "scrub status:1\n\
60b05268-7f8f-47b5-a38a-752576a1172a:1|data_extents_scrubbed:21123358|tree_extents_scrubbed:1053852|data_bytes_scrubbed:1077538492416|tree_bytes_scrubbed:17266311168|read_errors:0|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:1824300662784|t_start:1785182081|t_resumed:0|duration:6274|canceled:0|finished:1\n";

    /// The 2026-05-24 record's shape: aborted (`canceled:0|finished:0`),
    /// nothing scrubbed, super-block errors — silently treated as healthy for
    /// 64 days. Reconstructed in the on-disk field format (that filesystem's
    /// record was overwritten by the successful 2026-07-27 scrub), from the
    /// documented `Status: aborted` / `super=3` / `Total to scrub: 0.00B`
    /// rendering. The equivalent flag shape is still live on this host in
    /// `scrub.status.953ea709-…` and `scrub.status.fd01dde9-…`, both of which
    /// this parser reports as `aborted` with zero error counters.
    const REAL_ABORTED_SUPER: &str = "scrub status:1\n\
60b05268-7f8f-47b5-a38a-752576a1172a:1|data_extents_scrubbed:0|tree_extents_scrubbed:0|data_bytes_scrubbed:0|tree_bytes_scrubbed:0|read_errors:0|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:3|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:0|t_start:1779998400|t_resumed:0|duration:0|canceled:0|finished:0\n";

    /// Real two-device RAID-1 record (22 TB primary, both legs finished).
    const REAL_RAID1_FINISHED: &str = "scrub status:1\n\
b2dbe07d-40b9-422e-8ccf-ef4931c40457:1|data_extents_scrubbed:95871167|tree_extents_scrubbed:839307|data_bytes_scrubbed:5934387892224|tree_bytes_scrubbed:13751205888|read_errors:0|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:5960906113024|t_start:1779649863|t_resumed:0|duration:28701|canceled:0|finished:1\n\
b2dbe07d-40b9-422e-8ccf-ef4931c40457:2|data_extents_scrubbed:95871167|tree_extents_scrubbed:839220|data_bytes_scrubbed:5934387892224|tree_bytes_scrubbed:13749780480|read_errors:0|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:5961449275392|t_start:1779649863|t_resumed:0|duration:32027|canceled:0|finished:1\n";

    /// Real cancelled record (2026-05-06, filesystem 46ffbd7c-…): btrfs sets
    /// `canceled:1` *and* `finished:1`, plus 16 uncorrectable read errors.
    const REAL_CANCELED: &str = "scrub status:1\n\
46ffbd7c-dfd9-4ba5-82ae-0afffde99bb1:1|data_extents_scrubbed:414|tree_extents_scrubbed:95088|data_bytes_scrubbed:8388608|tree_bytes_scrubbed:1557921792|read_errors:16|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:16|corrected_errors:0|last_physical:1601830912|t_start:1778096820|t_resumed:0|duration:9419|canceled:1|finished:1\n";

    /// Real record with a huge informational `no_csum` count on a healthy
    /// filesystem (dasRaid0, nodatacow VM images).
    const REAL_NO_CSUM: &str = "scrub status:1\n\
d29fdda7-a1e5-4640-996e-2b78569cb65d:1|data_extents_scrubbed:10233933|tree_extents_scrubbed:2076|data_bytes_scrubbed:669937827840|tree_bytes_scrubbed:34013184|read_errors:0|csum_errors:0|verify_errors:0|no_csum:163559040|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:1163958157312|t_start:1776373270|t_resumed:0|duration:3027|canceled:0|finished:1\n";

    // --- parsing ---------------------------------------------------------

    #[test]
    fn parses_real_finished_record() {
        let rec =
            parse_scrub_status(REAL_FINISHED, "60b05268-7f8f-47b5-a38a-752576a1172a").unwrap();
        assert_eq!(rec.devices.len(), 1);
        assert_eq!(rec.outcome(), ScrubOutcome::Finished);
        assert!(!rec.counters().has_errors());
        assert!(rec.is_clean());
        assert_eq!(rec.bytes_scrubbed(), 1077538492416 + 17266311168);
        assert_eq!(rec.t_start(), 1785182081);
        assert_eq!(rec.duration_secs(), 6274);
        assert_eq!(rec.finished_epoch(), 1785182081 + 6274);
    }

    /// HARD REQUIREMENT 1: an aborted record must never read as healthy, even
    /// though its `read/csum/verify` counters are all zero.
    #[test]
    fn aborted_with_super_errors_is_not_clean() {
        let rec =
            parse_scrub_status(REAL_ABORTED_SUPER, "60b05268-7f8f-47b5-a38a-752576a1172a").unwrap();
        assert_eq!(rec.outcome(), ScrubOutcome::Aborted);
        assert_eq!(rec.counters().super_errors, 3);
        assert_eq!(rec.counters().error_total(), 3);
        assert!(rec.counters().has_errors());
        assert_eq!(rec.bytes_scrubbed(), 0);
        assert!(!rec.is_clean(), "aborted record must never be clean");
    }

    /// The same shape with *zero* error counters must still fail — the flags
    /// alone decide.
    #[test]
    fn aborted_without_any_error_counter_is_still_not_clean() {
        let content = REAL_ABORTED_SUPER.replace("super_errors:3", "super_errors:0");
        let rec = parse_scrub_status(&content, "60b05268-7f8f-47b5-a38a-752576a1172a").unwrap();
        assert_eq!(rec.outcome(), ScrubOutcome::Aborted);
        assert!(!rec.counters().has_errors());
        assert!(
            !rec.is_clean(),
            "an aborted scrub with clean counters is still a failure"
        );
    }

    #[test]
    fn canceled_record_reports_canceled_not_finished() {
        let rec =
            parse_scrub_status(REAL_CANCELED, "46ffbd7c-dfd9-4ba5-82ae-0afffde99bb1").unwrap();
        // btrfs sets finished:1 alongside canceled:1 — canceled must win.
        assert_eq!(rec.outcome(), ScrubOutcome::Canceled);
        assert_eq!(rec.counters().read_errors, 16);
        assert_eq!(rec.counters().uncorrectable_errors, 16);
        assert!(!rec.is_clean());
    }

    #[test]
    fn raid1_record_aggregates_both_devices() {
        let rec = parse_scrub_status(REAL_RAID1_FINISHED, "b2dbe07d-40b9-422e-8ccf-ef4931c40457")
            .unwrap();
        assert_eq!(rec.devices.len(), 2);
        assert_eq!(rec.outcome(), ScrubOutcome::Finished);
        assert_eq!(rec.duration_secs(), 32027, "longest device duration wins");
        assert_eq!(
            rec.bytes_scrubbed(),
            5934387892224 + 13751205888 + 5934387892224 + 13749780480
        );
        assert!(rec.is_clean());
    }

    #[test]
    fn raid1_with_one_aborted_leg_is_aborted() {
        let content =
            REAL_RAID1_FINISHED.replacen("canceled:0|finished:1", "canceled:0|finished:0", 1);
        let rec = parse_scrub_status(&content, "b2dbe07d-40b9-422e-8ccf-ef4931c40457").unwrap();
        assert_eq!(rec.outcome(), ScrubOutcome::Aborted);
        assert!(!rec.is_clean());
    }

    #[test]
    fn no_csum_is_informational_not_an_error() {
        let rec = parse_scrub_status(REAL_NO_CSUM, "d29fdda7-a1e5-4640-996e-2b78569cb65d").unwrap();
        assert_eq!(rec.counters().no_csum, 163559040);
        assert_eq!(rec.counters().error_total(), 0);
        assert!(rec.is_clean());
    }

    #[test]
    fn corrected_errors_count_as_damage() {
        let content = REAL_FINISHED.replace("corrected_errors:0", "corrected_errors:7");
        let rec = parse_scrub_status(&content, "60b05268-7f8f-47b5-a38a-752576a1172a").unwrap();
        assert_eq!(rec.outcome(), ScrubOutcome::Finished);
        assert_eq!(rec.counters().error_total(), 7);
        assert!(!rec.is_clean());
        assert!(rec.counters().summary().contains("corrected=7"));
    }

    // --- UUID-keyed resolution -------------------------------------------

    /// HARD REQUIREMENT 2: a record belonging to another filesystem is
    /// rejected, not attributed. This is the wrong-mount scenario — the NVMe
    /// root's record returned for a DAS query.
    #[test]
    fn record_for_a_different_filesystem_is_rejected() {
        // The NVMe root's real UUID, standing in for what an unmounted DAS
        // path resolves to.
        let nvme = REAL_FINISHED.replace(
            "60b05268-7f8f-47b5-a38a-752576a1172a",
            "20b5fa7e-d8c0-4035-ae45-f80263073a96",
        );
        let err = parse_scrub_status(&nvme, "60b05268-7f8f-47b5-a38a-752576a1172a")
            .expect_err("must not accept another filesystem's record");
        match err {
            ScrubError::StatusUuidMismatch { expected, found } => {
                assert_eq!(expected, "60b05268-7f8f-47b5-a38a-752576a1172a");
                assert_eq!(found, "20b5fa7e-d8c0-4035-ae45-f80263073a96");
            }
            other => panic!("expected UuidMismatch, got {other:?}"),
        }
    }

    #[test]
    fn status_record_path_is_keyed_by_uuid_only() {
        // No mount point appears anywhere in the resolved path.
        let path = status_record_path("60b05268-7f8f-47b5-a38a-752576a1172a");
        let s = path.display().to_string();
        assert!(s.ends_with("/scrub.status.60b05268-7f8f-47b5-a38a-752576a1172a"));
        assert!(!s.contains("/mnt/"));
    }

    #[test]
    fn read_scrub_status_reads_by_uuid_from_status_dir() {
        let dir = tempfile::tempdir().unwrap();
        let uuid = "60b05268-7f8f-47b5-a38a-752576a1172a";
        fs::write(
            dir.path().join(format!("scrub.status.{uuid}")),
            REAL_FINISHED,
        )
        .unwrap();
        // Also drop a decoy record for another filesystem in the same dir.
        fs::write(
            dir.path()
                .join("scrub.status.20b5fa7e-d8c0-4035-ae45-f80263073a96"),
            REAL_NO_CSUM,
        )
        .unwrap();

        temp_env("DAS_BTRFS_STATUS_DIR", dir.path().to_str().unwrap(), || {
            let rec = read_scrub_status(uuid).unwrap();
            assert_eq!(rec.fsuuid, uuid);
            assert_eq!(rec.outcome(), ScrubOutcome::Finished);
            // Missing UUID → explicit error, never a fallback to any other
            // record present in the directory.
            let err = read_scrub_status("00000000-0000-0000-0000-000000000000")
                .expect_err("missing record must error");
            assert!(matches!(err, ScrubError::StatusMissing { .. }));
        });
    }

    #[test]
    fn rejects_missing_header_and_unknown_version() {
        let err = parse_scrub_status("nonsense\n", "x").unwrap_err();
        assert!(matches!(err, ScrubError::StatusMissingHeader));

        let err = parse_scrub_status("scrub status:2\nx:1|finished:1\n", "x").unwrap_err();
        assert!(matches!(err, ScrubError::StatusUnsupportedVersion { .. }));
    }

    #[test]
    fn rejects_header_only_record() {
        let err = parse_scrub_status("scrub status:1\n", "abc").unwrap_err();
        assert!(matches!(err, ScrubError::StatusNoDevices { .. }));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let content = "scrub status:1\nabc:1|finished:1|canceled:0|brand_new_counter:42|t_start:100|duration:5\n";
        let rec = parse_scrub_status(content, "abc").unwrap();
        assert_eq!(rec.outcome(), ScrubOutcome::Finished);
        assert_eq!(rec.t_start(), 100);
    }

    // --- resumed scrubs / freshness anchor --------------------------------

    #[test]
    fn freshness_anchor_uses_the_later_of_start_and_resume() {
        // A resumed scrub keeps its original t_start; anchoring on t_start
        // alone would call a fresh resume "stale".
        let content = REAL_FINISHED.replace("t_resumed:0", "t_resumed:1785200000");
        let rec = parse_scrub_status(&content, "60b05268-7f8f-47b5-a38a-752576a1172a").unwrap();
        assert_eq!(rec.t_start(), 1785182081);
        assert_eq!(rec.devices[0].t_resumed, 1785200000);
        assert_eq!(rec.last_activity_start(), 1785200000);
    }

    #[test]
    fn freshness_anchor_falls_back_to_t_start_when_never_resumed() {
        let rec =
            parse_scrub_status(REAL_FINISHED, "60b05268-7f8f-47b5-a38a-752576a1172a").unwrap();
        assert_eq!(rec.last_activity_start(), rec.t_start());
    }

    // --- forced-start decision (C1) ---------------------------------------

    #[test]
    fn no_prior_record_starts_normally() {
        let dir = tempfile::tempdir().unwrap();
        temp_env("DAS_BTRFS_STATUS_DIR", dir.path().to_str().unwrap(), || {
            // Mount point is never consulted when there is no record to clear.
            assert_eq!(
                decide_scrub_start_mode("00000000-0000-0000-0000-000000000000", "/nonexistent"),
                ScrubStartMode::Normal
            );
        });
    }

    #[test]
    fn finished_prior_record_starts_normally() {
        let dir = tempfile::tempdir().unwrap();
        let uuid = "60b05268-7f8f-47b5-a38a-752576a1172a";
        fs::write(
            dir.path().join(format!("scrub.status.{uuid}")),
            REAL_FINISHED,
        )
        .unwrap();
        temp_env("DAS_BTRFS_STATUS_DIR", dir.path().to_str().unwrap(), || {
            assert_eq!(
                decide_scrub_start_mode(uuid, "/nonexistent"),
                ScrubStartMode::Normal
            );
        });
    }

    /// An unusable mount path yields `Unknown` liveness, which must never
    /// force — the engine only forces on positive evidence that nothing runs.
    #[test]
    fn aborted_prior_record_does_not_force_when_liveness_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let uuid = "60b05268-7f8f-47b5-a38a-752576a1172a";
        fs::write(
            dir.path().join(format!("scrub.status.{uuid}")),
            REAL_ABORTED_SUPER,
        )
        .unwrap();
        temp_env("DAS_BTRFS_STATUS_DIR", dir.path().to_str().unwrap(), || {
            // `/nonexistent` is not a btrfs mount, so `btrfs scrub status`
            // errors → Unknown → never force on a guess.
            assert_eq!(
                decide_scrub_start_mode(uuid, "/nonexistent"),
                ScrubStartMode::Normal,
                "must not force when liveness cannot be determined"
            );
        });
    }

    /// Verbatim `btrfs scrub status` output captured from the v7.1 loopback
    /// rig on 2026-08-01, one sample per state the engine must distinguish.
    #[test]
    fn live_scrub_state_parses_real_status_output() {
        let running = "UUID:             a5db734a-d7b4-4c5b-8ff6-13bc83b5f07d\n\
Scrub started:    Sat Aug  1 15:44:08 2026\n\
Status:           running\n\
Duration:         0:00:03\n\
Bytes scrubbed:   257.12MiB  (64.08%)\n";
        assert_eq!(parse_scrub_status_output(running), LiveScrubState::Running);

        let finished = "UUID:             a5db734a-d7b4-4c5b-8ff6-13bc83b5f07d\n\
Status:           finished\n\
Error summary:    no errors found\n";
        assert_eq!(
            parse_scrub_status_output(finished),
            LiveScrubState::NotRunning
        );

        // What v7.1 prints for a cancelled scrub (canceled:1) …
        let cancelled = "Status:           aborted\n";
        assert_eq!(
            parse_scrub_status_output(cancelled),
            LiveScrubState::NotRunning
        );
        // … and for the aborted-record shape (canceled:0 finished:0).
        let interrupted = "Status:           interrupted\n";
        assert_eq!(
            parse_scrub_status_output(interrupted),
            LiveScrubState::NotRunning
        );

        // A scrub that just started has no stats yet and prints no Status line
        // — that must read as Unknown, never as NotRunning, or the engine
        // could force-restart a live scrub.
        let no_stats = "UUID:             a5db734a-d7b4-4c5b-8ff6-13bc83b5f07d\n\
\tno stats available\n\
Total to scrub:   401.28MiB\n";
        assert_eq!(parse_scrub_status_output(no_stats), LiveScrubState::Unknown);
    }

    /// `live_scrub_state` must never trust a path that is not verifiably
    /// backed by the expected filesystem — the exact hazard the
    /// 2026-08-01 `bd DAS-Backup-Manager-0kn` review caught: an unmounted
    /// (or wrongly-mounted) DAS target's configured mount point is an
    /// ordinary directory that resolves through to whatever filesystem is
    /// actually there, and a caller that skipped UUID verification could
    /// mistake an unrelated live scrub for this filesystem's own.
    #[test]
    fn live_scrub_state_is_unknown_without_verified_mount() {
        // A path that does not exist at all can never verify.
        assert_eq!(
            live_scrub_state(
                "/nonexistent/das-scrub-uuid-check",
                "11111111-1111-1111-1111-111111111111"
            ),
            LiveScrubState::Unknown,
            "a nonexistent path must never be trusted"
        );

        // "/" is always a real, live mountpoint — findmnt will happily
        // report ITS uuid. The point of this assertion is that a REAL,
        // VERIFIABLE mount at the path is still `Unknown` when the caller's
        // expected UUID does not match what is actually mounted there: this
        // is precisely the scenario a `cancel` command must get right,
        // since a bare DAS target directory falling through to root (or any
        // other live filesystem) must never be read as "this DAS
        // filesystem's scrub".
        assert_eq!(
            live_scrub_state("/", "00000000-0000-0000-0000-000000000000"),
            LiveScrubState::Unknown,
            "a real mount with the WRONG uuid must never be trusted"
        );
    }

    // --- UUID resolution from config -------------------------------------

    /// A config carrying one target, since `Config::default()` has none.
    fn config_with_target(label: &str, mount_uuid: Option<&str>, serials: Vec<&str>) -> Config {
        use crate::config::{Retention, Target, TargetRole};
        let mut config = Config::default();
        config.targets.push(Target {
            label: label.into(),
            serial: serials
                .first()
                .map(|s| (*s).to_string())
                .unwrap_or_default(),
            serials: serials.iter().map(|s| (*s).to_string()).collect(),
            mount_uuid: mount_uuid.map(str::to_string),
            mount: format!("/mnt/{label}"),
            role: TargetRole::Primary,
            retention: Retention::default(),
            display_name: label.into(),
        });
        config
    }

    #[test]
    fn resolve_fsuuid_prefers_configured_mount_uuid() {
        // Serial resolution would need the physical drive; mount_uuid must win
        // without ever touching the block layer.
        let config = config_with_target(
            "primary-22tb",
            Some("b2dbe07d-40b9-422e-8ccf-ef4931c40457"),
            vec!["ZXA1NYGZ", "ZXA1R71M"],
        );
        assert_eq!(
            resolve_target_fsuuid(&config, "primary-22tb").unwrap(),
            "b2dbe07d-40b9-422e-8ccf-ef4931c40457"
        );
    }

    #[test]
    fn resolve_fsuuid_rejects_unknown_label() {
        let config = Config::default();
        let err = resolve_target_fsuuid(&config, "no-such-target").unwrap_err();
        assert!(err.contains("no-such-target"));
    }

    #[test]
    fn resolve_fsuuid_errors_when_nothing_identifies_the_filesystem() {
        let config = config_with_target("bare", None, vec![]);
        let err = resolve_target_fsuuid(&config, "bare").unwrap_err();
        assert!(err.contains("cannot resolve filesystem UUID"));
    }

    // --- locking ---------------------------------------------------------

    #[test]
    fn file_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let first = FileLock::try_acquire(&path).unwrap();
        assert!(first.is_some());
        // flock is per open-file-description, so a second open in this same
        // process contends exactly like a second process would.
        assert!(FileLock::try_acquire(&path).unwrap().is_none());

        drop(first);
        assert!(FileLock::try_acquire(&path).unwrap().is_some());
    }

    /// Lock order is singleton → maintenance, and a held singleton short-
    /// circuits before the maintenance lock is ever touched.
    #[test]
    fn held_singleton_skips_without_touching_maintenance_lock() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        let maintenance = dir.path().join("das-maintenance.lock");
        let progress = NullProgress;

        let held = FileLock::try_acquire(&singleton).unwrap().unwrap();
        let result = acquire_locks_at(&singleton, &maintenance, &progress).unwrap();
        assert!(result.is_none(), "must skip when singleton is held");
        assert!(
            !maintenance.exists(),
            "maintenance lock must not be opened when the singleton is unavailable"
        );

        drop(held);
        let locks = acquire_locks_at(&singleton, &maintenance, &progress)
            .unwrap()
            .expect("locks available");
        assert!(maintenance.exists());
        let (s, m) = locks.paths();
        assert_eq!(s, singleton.as_path());
        assert_eq!(m, maintenance.as_path());

        // Both locks are held: a second pass skips.
        assert!(
            acquire_locks_at(&singleton, &maintenance, &progress)
                .unwrap()
                .is_none()
        );

        drop(locks);
        assert!(
            acquire_locks_at(&singleton, &maintenance, &progress)
                .unwrap()
                .is_some(),
            "both locks must be released on drop"
        );
    }

    #[test]
    fn blocking_acquire_returns_immediately_when_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free.lock");
        let progress = TestProgress::new();
        let started = Instant::now();
        let lock = FileLock::acquire_blocking(&path, &progress, "waiting").unwrap();
        assert!(started.elapsed() < Duration::from_secs(LOCK_WAIT_ANNOUNCE_SECS));
        assert_eq!(lock.path(), path.as_path());
        // No wait announcement when there was no wait.
        assert!(progress.logs.lock().unwrap().is_empty());
    }

    // --- state file ------------------------------------------------------

    fn fs_result(label: &str, uuid: &str, ok: bool, finished_epoch: i64) -> ScrubFsResult {
        let mut r = ScrubFsResult::new(label);
        r.fsuuid = uuid.into();
        r.mountpoint = format!("/mnt/{label}");
        r.started_epoch = finished_epoch - 100;
        r.finished_epoch = finished_epoch;
        r.duration_secs = 100;
        r.bytes_scrubbed = 4096;
        if ok {
            r.outcome = Some(ScrubOutcome::Finished);
        } else {
            r.outcome = Some(ScrubOutcome::Aborted);
            r.counters.super_errors = 3;
        }
        r
    }

    fn pass_with(results: Vec<ScrubFsResult>) -> ScrubPass {
        ScrubPass {
            status: PassStatus::Completed,
            started_epoch: 1_000_000,
            finished_epoch: 1_000_500,
            results,
            errors: Vec::new(),
        }
    }

    #[test]
    fn state_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub-state.json");
        let pass = pass_with(vec![fs_result("primary-22tb", "uuid-a", true, 1_000_400)]);

        let mut state = load_state_from(&path).unwrap();
        merge_pass_into_state(&mut state, &pass);
        save_state_to(&state, &path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "health checks read this file unprivileged");

        let loaded = load_state_from(&path).unwrap();
        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        let fs_state = loaded.filesystems.get("uuid-a").unwrap();
        assert_eq!(fs_state.target_label, "primary-22tb");
        assert_eq!(fs_state.last_success_epoch, Some(1_000_400));
        assert_eq!(fs_state.last_attempt.outcome, "finished");
        assert!(fs_state.last_attempt.ok);
        let summary = loaded.last_pass.unwrap();
        assert!(summary.success);
        assert_eq!(summary.targets_attempted, 1);
        assert_eq!(summary.targets_failed, 0);
    }

    #[test]
    fn failed_pass_preserves_previous_success_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub-state.json");

        let mut state = ScrubState::default();
        merge_pass_into_state(
            &mut state,
            &pass_with(vec![fs_result("recovery-a", "uuid-b", true, 1_000_400)]),
        );
        save_state_to(&state, &path).unwrap();

        let mut state = load_state_from(&path).unwrap();
        merge_pass_into_state(
            &mut state,
            &pass_with(vec![fs_result("recovery-a", "uuid-b", false, 2_000_400)]),
        );
        save_state_to(&state, &path).unwrap();

        let loaded = load_state_from(&path).unwrap();
        let fs_state = loaded.filesystems.get("uuid-b").unwrap();
        assert_eq!(
            fs_state.last_success_epoch,
            Some(1_000_400),
            "a failed attempt must not erase the last known good scrub"
        );
        assert_eq!(fs_state.last_attempt.outcome, "aborted");
        assert!(!fs_state.last_attempt.ok);
        assert_eq!(fs_state.last_attempt.error_total, 3);
        assert!(!loaded.last_pass.unwrap().success);
    }

    #[test]
    fn missing_state_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let state = load_state_from(&dir.path().join("absent.json")).unwrap();
        assert!(state.filesystems.is_empty());
        assert!(state.last_pass.is_none());
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
    }

    #[test]
    fn corrupt_state_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub-state.json");
        fs::write(&path, "{ this is not json").unwrap();
        let err = load_state_from(&path).unwrap_err();
        assert!(matches!(err, ScrubError::StateCorrupt { .. }));
        // Loading is pure: the corrupt file is still exactly where it was, so
        // an unprivileged health check cannot mutate the writer's state.
        assert!(path.exists(), "load_state_from must not move the file");
        assert!(!dir.path().join("scrub-state.json.corrupt").exists());

        // Quarantine is a separate, explicit, writer-side call.
        let moved = quarantine_state_file(&path).unwrap();
        assert_eq!(moved, dir.path().join("scrub-state.json.corrupt"));
        assert!(!path.exists());
        assert!(moved.exists());
    }

    /// A corrupt file is already moved aside, so this pass's result is still
    /// written — losing the pass would be the worse failure.
    #[test]
    fn persist_writes_fresh_state_over_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub-state.json");
        fs::write(&path, "{ not json").unwrap();
        let pass = pass_with(vec![fs_result("primary-22tb", "uuid-a", true, 1_000_400)]);

        temp_env("DAS_SCRUB_STATE", path.to_str().unwrap(), || {
            persist_pass(&pass, &NullProgress).expect("corrupt state must not block persistence");
        });

        let loaded = load_state_from(&path).unwrap();
        assert!(loaded.filesystems.contains_key("uuid-a"));
        assert!(dir.path().join("scrub-state.json.corrupt").exists());
    }

    /// An intact-but-unreadable state file must never be overwritten from a
    /// blank state — that would erase every `last_success_epoch` health ages
    /// against, silently.
    #[test]
    fn persist_refuses_to_overwrite_an_unreadable_state_file() {
        // SAFETY: geteuid() is always safe. Mode 0000 does not stop root, so
        // the premise of this test only holds unprivileged.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, mode 0000 does not deny reads");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub-state.json");

        let mut state = ScrubState::default();
        merge_pass_into_state(
            &mut state,
            &pass_with(vec![fs_result("recovery-a", "uuid-b", true, 1_000_400)]),
        );
        save_state_to(&state, &path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let pass = pass_with(vec![fs_result("recovery-a", "uuid-b", false, 2_000_400)]);
        let err = temp_env("DAS_SCRUB_STATE", path.to_str().unwrap(), || {
            persist_pass(&pass, &NullProgress).expect_err("unreadable state must abort the write")
        });
        assert!(matches!(err, ScrubError::State { .. }));

        // The original file is untouched, history intact.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let loaded = load_state_from(&path).unwrap();
        assert_eq!(
            loaded.filesystems.get("uuid-b").unwrap().last_success_epoch,
            Some(1_000_400)
        );
    }

    #[test]
    fn unresolved_uuid_is_not_written_to_state() {
        let mut r = ScrubFsResult::new("broken");
        r.errors.push("drive not present".into());
        let mut state = ScrubState::default();
        merge_pass_into_state(&mut state, &pass_with(vec![r]));
        assert!(state.filesystems.is_empty());
        assert_eq!(state.last_pass.unwrap().targets_failed, 1);
    }

    // --- result semantics + report ---------------------------------------

    #[test]
    fn engine_error_fails_the_filesystem_even_when_the_record_is_clean() {
        let mut r = fs_result("primary-22tb", "uuid-a", true, 1_000_400);
        assert!(r.ok());
        r.errors
            .push("umount /mnt/backup-22tb failed: target is busy".into());
        assert!(!r.ok(), "unmount failure must not be silent");
        assert_eq!(r.status_word(), "FAILED");
    }

    #[test]
    fn status_word_distinguishes_aborted_from_errors() {
        let mut aborted = fs_result("a", "u", false, 10);
        aborted.counters = ScrubCounters::default();
        assert_eq!(aborted.status_word(), "ABORTED");

        let mut with_errors = fs_result("b", "u2", true, 10);
        with_errors.counters.csum_errors = 2;
        assert_eq!(with_errors.status_word(), "ERRORS");
    }

    #[test]
    fn report_marks_failure_and_lists_problems() {
        let config = Config::default();
        let pass = pass_with(vec![
            fs_result("primary-22tb", "uuid-a", true, 1_000_400),
            fs_result("system-recovery-A-2tb", "uuid-b", false, 1_000_450),
        ]);
        let report = format_scrub_report(&pass, &config);
        assert!(report.contains("FAILURES DETECTED"));
        assert!(report.contains("system-recovery-A-2tb"));
        assert!(report.contains("ABORTED"));
        assert!(report.contains("super=3"));
        assert!(report.contains("PROBLEMS"));
        assert!(report.contains("uuid=uuid-b"));
    }

    #[test]
    fn clean_report_has_no_failure_marker() {
        let config = Config::default();
        let pass = pass_with(vec![fs_result("primary-22tb", "uuid-a", true, 1_000_400)]);
        let report = format_scrub_report(&pass, &config);
        assert!(report.contains("ALL SCRUBS COMPLETED CLEANLY"));
        assert!(!report.contains("FAILURE"));
        assert!(!report.contains("PROBLEMS"));
    }

    #[test]
    fn skipped_pass_is_a_success_and_reports_as_skipped() {
        let config = Config::default();
        let pass = ScrubPass {
            status: PassStatus::Skipped,
            started_epoch: 1,
            finished_epoch: 2,
            results: Vec::new(),
            errors: Vec::new(),
        };
        assert!(pass.success());
        assert!(format_scrub_report(&pass, &config).contains("SKIPPED"));
    }

    #[test]
    fn run_scrub_pass_errors_when_no_targets_configured() {
        let mut config = Config::default();
        config.scrub.targets.clear();
        let err = run_scrub_pass(&config, &NullProgress).unwrap_err();
        assert!(matches!(err, ScrubError::NoTargets));
    }

    #[test]
    fn byte_and_duration_formatting() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1_099_511_627_776), "1.00 TiB");
        assert_eq!(format_duration(6274), "1h 44m");
        assert_eq!(format_duration(90), "1m 30s");
    }

    /// Set an env var for the duration of `f`, restoring it afterwards.
    /// Serialized by a mutex because env vars are process-global.
    fn temp_env<R>(key: &str, value: &str, f: impl FnOnce() -> R) -> R {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(key).ok();
        // SAFETY: all env mutation in this module's tests is serialized by
        // ENV_LOCK, and no other thread reads these variables concurrently.
        unsafe { std::env::set_var(key, value) };
        let out = f();
        unsafe {
            match previous {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        out
    }
}
