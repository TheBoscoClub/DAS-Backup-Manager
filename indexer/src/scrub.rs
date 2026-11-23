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
//! Both come out of a 2026-07-27 investigation of one recovery target, where a
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
//! An `Aborted` record (device row neither `finished` nor `canceled`) is a
//! scrub interrupted by a reboot or an unmount, and it carries a resumable
//! position. [`decide_scrub_start_mode`] returns `Resume` **only** when the
//! prior record is `Aborted` *and* the kernel confirms nothing is running —
//! never on a guess, because these locks cannot stop an operator from starting
//! a scrub by hand. If the kernel then has no resumable state after all
//! (`btrfs scrub resume` prints "nothing to resume"), the runner falls back to
//! `start -B -f` (every argv also carries the priority flags `-c 1 -n 0` or
//! `-c 2 -n 0`, see `ScrubIoClass`), which also clears the genuinely stale record that
//! `btrfs-progs` would otherwise read as a live scrub and refuse to start over
//! ("useful when scrub status file is damaged and reports a running scrub
//! although it is not", `man btrfs-scrub` on `-f`). So one aborted pass can
//! neither be silently discarded nor wedge that filesystem's scrubs.
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
//! * **A second mount elsewhere is tolerated.** `btrdasd setup` hides the
//!   targets from udisks (bd `DAS-Backup-Manager-xqo`), so a file manager no
//!   longer mounts them under `/run/media/…`; if some other mount exists
//!   anyway it is not cleaned up. BTRFS permits the same filesystem at several
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
//! * `/run/das-scrub.lock` — non-blocking, holding its holder's record
//!   (`btrdasd scrub run pid N`). Held by another pass, or by a holder that
//!   recorded nothing, this invocation **skips** (logs the holder and returns
//!   [`PassStatus::Skipped`]); it never queues. Held by `btrdasd setup`, which
//!   takes it while it writes the installed files (bd DAS-Backup-Manager-su1b.2),
//!   the pass waits up to [`SETUP_WAIT_SECS`] for it, saying so, and skips with
//!   a WARN that it did NOT run if setup still holds it.
//! * `/run/das-maintenance.lock` — shared with `backup-run.sh` and every job
//!   that mounts a target; only its holder mounts or unmounts. The pass holds
//!   it while it scrubs, and **gives it up to a backup that claims it**
//!   (bd DAS-Backup-Manager-su1b.2): a backup waiting for the lock holds
//!   `/run/das-maintenance-claim.lock` shared ([`crate::maintenance::Claim`]),
//!   and the pass, which probes that file at every poll and before every
//!   mount, stops its `btrfs scrub` child (SIGTERM, reaped, so the record
//!   stays resumable), confirms the kernel stopped, unmounts what it mounted,
//!   writes a `yielded` record, and only then lets the lock go. It waits
//!   (the singleton still held) until nobody claims, takes the lock again,
//!   reads `config.toml` again, and resumes the same filesystem with
//!   `btrfs scrub resume`. A claim seen before the pass first takes the lock
//!   lets the backup go first. A backup never waits a whole pass; the pass
//!   ends later by the backups' length. Nothing else makes it yield.
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
//!     "5eed0008-0000-4000-8000-000000000008": {
//!       "target_label": "recovery-a",
//!       "mountpoint": "/mnt/backup-recovery-a",
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
//! * `last_attempt.outcome` is one of `finished`, `canceled`, `aborted`,
//!   `interrupted` (the pass caught SIGTERM, SIGINT or SIGHUP: `signal` names
//!   it), `running`, or `error` (the pass could not get far enough to produce a
//!   record at all). `running` is written before the scrub starts, with the
//!   writer's `pid` and `boot_id`; it is replaced by the result when the
//!   filesystem is done. A `running` record whose process is gone, or from an
//!   earlier boot, is read as `interrupted` ([`AttemptState::effective_outcome`]):
//!   the pass was killed without a chance to say so (bd
//!   DAS-Backup-Manager-weo).
//!   `yielded` (bd DAS-Backup-Manager-su1b.2) means the pass gave the
//!   maintenance lock up to a waiting backup and waits to resume the same
//!   filesystem; it carries `pid` and `boot_id` like `running`, plus the
//!   optional `yielded_to` (the claim's holder record), `yielded_epoch`,
//!   `yields` (count this pass) and `paused_secs` (total before this yield).
//!   Readers treat it as `running`: in progress while its process lives in
//!   this boot, `interrupted` otherwise. The pass writes it as it gives the
//!   lock up, for the filesystem it paused (or, for a pause before a mount,
//!   the one next in line), and replaces it with `running` on resuming.
//!   `paused` (bd DAS-Backup-Manager-su1b.15) is a scrub ended by
//!   `btrdasd scrub pause`: `yielded_to` names who asked and `yielded_epoch`
//!   when it paused. It is not in progress, never read as `interrupted`, and
//!   not a failure; `btrdasd scrub resume` resumes it.
//!   `interrupted` files the counters and bytes the btrfs status record held at
//!   the interruption (bd DAS-Backup-Manager-su1b.22); when that record is
//!   missing, unreadable or not this run's, and for every `paused` attempt,
//!   the optional `counters_unknown: true` marks `bytes_scrubbed`,
//!   `error_total` and `counters` as placeholders: readers print `unknown`,
//!   never 0 or "no errors". Absent (older files) means measured.
//!   `ok` is the single authoritative pass/fail bit: `outcome == "finished"`
//!   **and** no error counters **and** no engine-level errors.
//! * `paused_pass` (optional, bd DAS-Backup-Manager-su1b.24) is present only
//!   while a pass ended by `btrdasd scrub pause` waits to be resumed:
//!   `origin_epoch` (its first part's start), `paused_epoch`, and `finished`
//!   (FS UUIDs scrubbed to the end in it). Absent in older files.
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
//! directory and the state file, mirroring the `DAS_REPORT_TO` /
//! `DAS_REPORT_FROM` overrides in [`crate::report`]. Production leaves all of
//! them unset, and a shipped build does not read them at all: they exist only
//! under the `test-overrides` feature, which only the dev-dependency enables.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::maintenance::MaintenanceHeld;
use crate::mount::partition_device;
use crate::progress::{LogLevel, ProgressCallback};

/// Non-blocking singleton lock — a second scrub pass skips rather than queues.
/// `btrdasd setup` holds it too while it writes; a pass started then waits for
/// it ([`SETUP_WAIT_SECS`]).
pub const SCRUB_LOCK_PATH: &str = "/run/das-scrub.lock";
/// Blocking maintenance lock, shared with `backup-run.sh`.
pub const MAINTENANCE_LOCK_PATH: &str = "/run/das-maintenance.lock";
/// Default directory holding `scrub.status.<fsuuid>` records.
pub const BTRFS_STATUS_DIR: &str = "/var/lib/btrfs";
/// Default path of the persisted scrub state consumed by health checks.
pub const SCRUB_STATE_PATH: &str = "/var/lib/das-backup/scrub-state.json";
/// Schema version written to (and expected in) the state file.
pub const STATE_SCHEMA_VERSION: u32 = 1;
/// The pause request `btrdasd scrub pause` leaves (bd DAS-Backup-Manager-su1b.15):
/// while it exists a running pass yields and ends `paused`, and a pass that
/// starts does not run. Only `btrdasd scrub resume` (or a reboot, which empties
/// `/run`) takes it away; the `paused` record in the state file stays either way.
pub const SCRUB_PAUSE_REQUEST_PATH: &str = "/run/das-scrub-pause";
/// How long `btrdasd scrub pause` waits for the pass to end before it says so.
pub const PAUSE_ANSWER_SECS: u64 = 240;

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
    /// Nothing to scrub: `[scrub].targets` is empty and no `[[target]]` is
    /// configured (or the config is a recovery drive's own).
    NoTargets,
    /// A lock file could not be opened or locked.
    Lock { path: String, detail: String },
    /// The state file could not be read or written.
    State { path: String, detail: String },
    /// The state file holds unparsable JSON. Distinct from
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
    /// `config.toml` could not be read again once the pass held its locks.
    ConfigReload { detail: String },
}

impl fmt::Display for ScrubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTargets => write!(
                f,
                "No scrub targets: [scrub].targets is empty and no [[target]] is configured"
            ),
            Self::Lock { path, detail } => write!(f, "Lock '{path}': {detail}"),
            Self::State { path, detail } => write!(f, "Scrub state '{path}': {detail}"),
            Self::StateCorrupt { path, detail } => {
                write!(f, "Scrub state '{path}' is unparsable: {detail}")
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
            Self::ConfigReload { detail } => write!(
                f,
                "config.toml could not be read again once the scrub locks were held: {detail}"
            ),
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
        // record for filesystem 5eed0007-…).
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
            //
            // A RECOGNISED key whose value will not parse is the opposite case
            // and is refused. It used to fall back to 0, and 0 on an error
            // counter reads as "no damage" — so a truncated or corrupted status
            // record was reported as a CLEAN scrub: `is_clean()` true, no
            // FAILURE email, health green. The one answer this parser must
            // never fabricate (bd DAS-Backup-Manager-8wx).
            let malformed = || ScrubError::StatusMalformed {
                detail: format!("field '{key}' has non-numeric value '{value}'"),
            };
            match key {
                "read_errors" => {
                    dev.counters.read_errors = value.parse().map_err(|_| malformed())?
                }
                "csum_errors" => {
                    dev.counters.csum_errors = value.parse().map_err(|_| malformed())?
                }
                "verify_errors" => {
                    dev.counters.verify_errors = value.parse().map_err(|_| malformed())?
                }
                "super_errors" => {
                    dev.counters.super_errors = value.parse().map_err(|_| malformed())?
                }
                "malloc_errors" => {
                    dev.counters.malloc_errors = value.parse().map_err(|_| malformed())?
                }
                "uncorrectable_errors" => {
                    dev.counters.uncorrectable_errors = value.parse().map_err(|_| malformed())?
                }
                "corrected_errors" => {
                    dev.counters.corrected_errors = value.parse().map_err(|_| malformed())?
                }
                "no_csum" => dev.counters.no_csum = value.parse().map_err(|_| malformed())?,
                "csum_discards" => {
                    dev.counters.csum_discards = value.parse().map_err(|_| malformed())?
                }
                "data_bytes_scrubbed" => {
                    dev.data_bytes_scrubbed = value.parse().map_err(|_| malformed())?
                }
                "tree_bytes_scrubbed" => {
                    dev.tree_bytes_scrubbed = value.parse().map_err(|_| malformed())?
                }
                "t_start" => dev.t_start = value.parse::<i64>().map_err(|_| malformed())?,
                "t_resumed" => dev.t_resumed = value.parse::<i64>().map_err(|_| malformed())?,
                "duration" => dev.duration_secs = value.parse().map_err(|_| malformed())?,
                "canceled" => dev.canceled = value.parse::<u64>().map_err(|_| malformed())? != 0,
                "finished" => dev.finished = value.parse::<u64>().map_err(|_| malformed())? != 0,
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

/// `default`, unless a test build (feature `test-overrides`, enabled only
/// through the dev-dependency) sets `var`. A shipped binary never reads the
/// environment here: a stray variable must not redirect the scrub state.
fn overridable(var: &str, default: &str) -> PathBuf {
    if cfg!(feature = "test-overrides") {
        PathBuf::from(std::env::var(var).unwrap_or_else(|_| default.to_string()))
    } else {
        PathBuf::from(default)
    }
}

/// The claim lock a backup takes to ask a pass to let it go first
/// ([`crate::maintenance::CLAIM_LOCK_PATH`], overridable for tests).
fn claim_lock_path() -> PathBuf {
    overridable("DAS_CLAIM_LOCK", crate::maintenance::CLAIM_LOCK_PATH)
}

/// The pause request ([`SCRUB_PAUSE_REQUEST_PATH`], overridable for tests).
pub fn pause_request_path() -> PathBuf {
    overridable("DAS_SCRUB_PAUSE_REQUEST", SCRUB_PAUSE_REQUEST_PATH)
}

/// Directory holding the btrfs status records (overridable for tests).
fn btrfs_status_dir() -> PathBuf {
    overridable("DAS_BTRFS_STATUS_DIR", BTRFS_STATUS_DIR)
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
/// Let go on drop by an explicit `LOCK_UN`, then the close. The close alone
/// would not let go while a copy of the descriptor lives, and a fork of this
/// process copies every descriptor until its exec — every spawn does, std's
/// `posix_spawn` included. `flock` lets go once every copy is closed, or at
/// a `LOCK_UN` through any one of them (bd DAS-Backup-Manager-eu0). It also
/// lets go if the `File` is leaked into a longer-lived structure.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
    file: File,
}

/// Every lock file this process opened to lock it, by path, so a test can
/// count the attempts on its own scratch lock — a free lock is taken at the
/// first attempt, never probed first (`maintenance` tests).
#[cfg(test)]
pub(crate) mod opens {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static TAKEN: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(crate) fn record(path: &Path) {
        TAKEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(path.to_path_buf());
    }

    pub(crate) fn count(path: &Path) -> usize {
        TAKEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|taken| taken.as_path() == path)
            .count()
    }
}

impl FileLock {
    fn open(path: &Path) -> Result<File, ScrubError> {
        #[cfg(test)]
        opens::record(path);
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent).map_err(|e| ScrubError::Lock {
                path: path.display().to_string(),
                detail: format!("cannot create lock directory: {e}"),
            })?;
        }
        // Root's alone: flock works on a read-only descriptor, so a lock file
        // anyone can open is one anyone can hold, keeping the nightly backup
        // and scrub waiting (bd DAS-Backup-Manager-qab). 0600 for a new file;
        // one an older version left 0644 is narrowed below.
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|e| {
                // `/run` is root-owned, so a permission error here almost
                // always means the caller simply is not root. Say that,
                // instead of surfacing a bare EACCES. Not only the scrub
                // opens these files: every job that mounts a backup target
                // takes the maintenance lock (bd DAS-Backup-Manager-frb).
                let denied = matches!(
                    e.raw_os_error(),
                    Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::EROFS)
                );
                // SAFETY: geteuid() is always safe.
                let unprivileged = unsafe { libc::geteuid() } != 0;
                let detail = if denied && unprivileged {
                    format!("cannot open lock file ({e}) — this command must run as root (sudo)")
                } else {
                    format!("cannot open lock file: {e}")
                };
                ScrubError::Lock {
                    path: path.display().to_string(),
                    detail,
                }
            })?;
        let narrow = |e: std::io::Error| ScrubError::Lock {
            path: path.display().to_string(),
            detail: format!("cannot make the lock file root's alone (0600): {e}"),
        };
        // A regular file only: never chmod a device node a path names.
        let meta = file.metadata().map_err(narrow)?;
        if meta.is_file() && meta.permissions().mode() & 0o077 != 0 {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(narrow)?;
        }
        Ok(file)
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

    /// Take the lock shared (`LOCK_SH`), waiting as long as necessary: for
    /// the claim lock (`maintenance::Claim`), where the only exclusive taker
    /// is a probe that lets go at once.
    pub(crate) fn acquire_shared(path: &Path) -> Result<FileLock, ScrubError> {
        let file = Self::open(path)?;
        // A blocking request returns only once held, or on a real error;
        // EINTR is retried inside `flock`.
        Self::flock(&file, path, libc::LOCK_SH)?;
        Ok(FileLock {
            path: path.to_path_buf(),
            file,
        })
    }

    /// Take the lock, waiting as long as necessary.
    ///
    /// A short non-blocking probe runs first so that ordinary sub-second
    /// contention stays quiet; once the wait passes
    /// [`LOCK_WAIT_ANNOUNCE_SECS`] a visible line is logged and the call falls
    /// through to a genuinely blocking `flock`. The line is built only then,
    /// so it can say who holds the lock at that moment.
    pub fn acquire_blocking(
        path: impl AsRef<Path>,
        progress: &dyn ProgressCallback,
        waiting_message: &dyn Fn() -> String,
    ) -> Result<FileLock, ScrubError> {
        let path = path.as_ref();
        if let Some(lock) = Self::try_acquire(path)? {
            return Ok(lock);
        }
        std::thread::sleep(Duration::from_secs(LOCK_WAIT_ANNOUNCE_SECS));
        if let Some(lock) = Self::try_acquire(path)? {
            return Ok(lock);
        }
        let waiting_message = waiting_message();
        progress.on_log(LogLevel::Info, &waiting_message);

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

    /// [`acquire_blocking`](Self::acquire_blocking) for a job that can be
    /// cancelled (bd DAS-Backup-Manager-yq2): a blocking `flock` cannot be
    /// interrupted, so this looks again every `poll` and stops as soon as
    /// `stop()` says so — `Ok(None)`, holding nothing. Logs `waiting_message`
    /// once, when it first finds the lock held.
    pub fn acquire_until(
        path: impl AsRef<Path>,
        progress: &dyn ProgressCallback,
        waiting_message: &dyn Fn() -> String,
        stop: &dyn Fn() -> bool,
        poll: Duration,
    ) -> Result<Option<FileLock>, ScrubError> {
        let path = path.as_ref();
        if let Some(lock) = Self::try_acquire(path)? {
            return Ok(Some(lock));
        }
        progress.on_log(LogLevel::Info, &waiting_message());
        loop {
            if stop() {
                return Ok(None);
            }
            std::thread::sleep(poll);
            if let Some(lock) = Self::try_acquire(path)? {
                return Ok(Some(lock));
            }
        }
    }

    /// Path of the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record in the lock file itself who holds the lock, replacing what an
    /// earlier holder left, so a process that finds it held can say what it
    /// is waiting for. Only the holder writes: every opener uses
    /// `truncate(false)`, and `backup-run.sh` opens with `<>`, so nobody
    /// waiting erases it.
    pub(crate) fn write_note(&self, note: &str) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.set_len(0)?;
        self.file.write_all_at(note.as_bytes(), 0)
    }

    /// Empty the lock file again — the holder is letting go.
    pub(crate) fn clear_note(&self) -> std::io::Result<()> {
        self.file.set_len(0)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // `LOCK_UN`, not the close alone: a fork's copy of the descriptor
        // would hold the lock on (see the type's doc).
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
///
/// The singleton carries its holder record (`btrdasd scrub run pid N`),
/// emptied while still held as the value drops, as the backup singleton's
/// does — so `btrdasd setup` and `scrub cancel` can tell a pass from setup's
/// own hold (bd DAS-Backup-Manager-su1b.2).
///
/// The maintenance hold is a [`MaintenanceSlot`]: the pass gives it up to a
/// backup that claims it and takes it back, holding the singleton (and its
/// termination handlers) throughout (bd DAS-Backup-Manager-su1b.2).
#[derive(Debug)]
pub struct ScrubLocks {
    maintenance: MaintenanceSlot,
    singleton: crate::backup::SingletonHeld,
}

impl ScrubLocks {
    /// Paths of the locks, in acquisition order.
    pub fn paths(&self) -> (&Path, &Path) {
        (self.singleton.path(), self.maintenance.path())
    }

    /// The maintenance hold, while the pass has it.
    pub fn maintenance(&self) -> Option<&MaintenanceHeld> {
        self.maintenance.held()
    }
}

/// How long a scrub pass that finds its singleton held by `btrdasd setup`
/// waits for setup to let go before it skips — `backup-run.sh`'s
/// `SINGLETON_WAIT_SECS` for setup and configuration edits (bd
/// DAS-Backup-Manager-al9, DAS-Backup-Manager-su1b.2).
pub const SETUP_WAIT_SECS: u64 = 900;

/// How often that wait looks at the singleton again.
const SETUP_WAIT_POLL: Duration = Duration::from_secs(5);

/// How a holder record of `btrdasd setup` begins (`btrdasd setup pid 4242`,
/// `btrdasd setup --upgrade pid 4242`), as `backup-run.sh` matches it.
pub const SETUP_HOLDER_PREFIX: &str = "btrdasd setup";

/// The job a scrub pass records itself as, in both of its locks.
const SCRUB_JOB: &str = "btrdasd scrub run";

/// Whether `holder`, as [`crate::maintenance::holder_of`] names a lock's
/// holder, is a live `btrdasd setup`: `holder_of` gives the record's line only
/// while its pid runs, so a dead setup's record reads as unknown.
pub fn holder_is_setup(holder: &str) -> bool {
    holder.starts_with(SETUP_HOLDER_PREFIX)
}

/// How a scrub pass found its locks.
#[derive(Debug)]
pub enum ScrubLockAttempt {
    /// Both taken, and held until the value is dropped.
    Taken(ScrubLocks),
    /// The singleton is held by `holder` (as its record names it), which is
    /// not `btrdasd setup`: another pass, or a holder that recorded nothing.
    Held(String),
    /// `btrdasd setup` (`holder`) still held the singleton once
    /// [`SETUP_WAIT_SECS`] had passed: this pass did not run.
    SetupOutlasted(String),
}

/// What one look at the singleton found.
enum SingletonLook {
    Taken(crate::backup::SingletonHeld),
    HeldBy(String),
}

/// Try the singleton at `path` once, recording this pass on success; held,
/// name its holder. A holder whose record names nobody is looked at once more
/// after `grace`: a holder letting go empties its record while it still holds
/// the lock, and that instant must not read as "someone else".
fn look_at_singleton(path: &Path, grace: Duration) -> Result<SingletonLook, ScrubError> {
    for attempt in 0..2 {
        if let Some(held) = crate::backup::SingletonHeld::try_acquire_at(path, SCRUB_JOB)? {
            return Ok(SingletonLook::Taken(held));
        }
        let holder = crate::maintenance::holder_of(path);
        if attempt == 1 || holder != crate::maintenance::UNKNOWN_HOLDER {
            return Ok(SingletonLook::HeldBy(holder));
        }
        std::thread::sleep(grace);
    }
    unreachable!("the second look always returns")
}

/// A wait, as the journal says it.
fn wait_words(wait: Duration) -> String {
    let secs = wait.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else if secs >= 1 {
        format!("{secs} s")
    } else {
        format!("{} ms", wait.as_millis())
    }
}

/// Acquire the singleton lock (non-blocking) then the maintenance lock
/// (blocking), recorded as held by `btrdasd scrub run`. A singleton held by
/// `btrdasd setup` is waited for, up to `setup_wait`, looked at every `poll`,
/// saying whom it waits for (bd DAS-Backup-Manager-su1b.2, as `backup-run.sh`
/// waits for setup, bd DAS-Backup-Manager-al9); held by anything else, or
/// still by setup once the wait is over, the pass does not run.
fn acquire_locks_at(
    singleton_path: &Path,
    maintenance_path: &Path,
    claim_path: &Path,
    progress: &dyn ProgressCallback,
    setup_wait: Duration,
    poll: Duration,
) -> Result<ScrubLockAttempt, ScrubError> {
    let grace = poll.min(Duration::from_millis(100));
    let singleton = match look_at_singleton(singleton_path, grace)? {
        SingletonLook::Taken(held) => held,
        SingletonLook::HeldBy(holder) if !holder_is_setup(&holder) => {
            return Ok(ScrubLockAttempt::Held(holder));
        }
        SingletonLook::HeldBy(mut holder) => {
            progress.on_log(
                LogLevel::Info,
                &format!(
                    "{} is held by {holder}, which is writing the installed files — waiting up \
                     to {} for it",
                    singleton_path.display(),
                    wait_words(setup_wait)
                ),
            );
            let deadline = Instant::now() + setup_wait;
            loop {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(ScrubLockAttempt::SetupOutlasted(holder));
                }
                std::thread::sleep(poll.min(deadline - now));
                match look_at_singleton(singleton_path, grace)? {
                    SingletonLook::Taken(held) => {
                        progress.on_log(
                            LogLevel::Info,
                            &format!("{holder} has let go of {}", singleton_path.display()),
                        );
                        break held;
                    }
                    SingletonLook::HeldBy(next) if holder_is_setup(&next) => holder = next,
                    // Setup let go and something else took it first.
                    SingletonLook::HeldBy(next) => return Ok(ScrubLockAttempt::Held(next)),
                }
            }
        }
    };
    // Checkpoints 1 and 2: a backup that claims the lock goes first.
    let claim = ClaimWatch::new(claim_path);
    let held = match take_maintenance_behind_claims(
        maintenance_path,
        poll,
        &|| claim.still_waiting(progress),
        &|| claim.wants_lock(progress),
        &|| None,
        // The pass's start never waits on a pause request: one in place
        // before the locks ends the pass earlier (`paused_at_start`), and a
        // later one is met at the first checkpoint.
        &|| None,
        progress,
    )? {
        Retake::Held(held) => held,
        Retake::Paused(_) => unreachable!("the start's wait is given no pause request"),
        // `stop` never answers here: the handlers are not installed yet, so
        // a signal ends the process as it always did, holding nothing.
        Retake::Interrupted(signal) => {
            return Err(ScrubError::Lock {
                path: maintenance_path.display().to_string(),
                detail: format!("interrupted by {}", signal_name(signal)),
            });
        }
    };
    Ok(ScrubLockAttempt::Taken(ScrubLocks {
        maintenance: MaintenanceSlot {
            path: maintenance_path.to_path_buf(),
            poll,
            held: Some(held),
        },
        singleton,
    }))
}

/// Acquire the production scrub locks.
pub fn acquire_locks(progress: &dyn ProgressCallback) -> Result<ScrubLockAttempt, ScrubError> {
    acquire_locks_at(
        &overridable("DAS_SCRUB_LOCK", SCRUB_LOCK_PATH),
        &overridable("DAS_MAINTENANCE_LOCK", MAINTENANCE_LOCK_PATH),
        &claim_lock_path(),
        progress,
        Duration::from_secs(SETUP_WAIT_SECS),
        SETUP_WAIT_POLL,
    )
}

/// What a pass that did not get its locks says: the journal line, at its
/// level, and the completion summary.
fn skipped_lines(attempt: &ScrubLockAttempt) -> (LogLevel, String, String) {
    match attempt {
        ScrubLockAttempt::SetupOutlasted(holder) => (
            LogLevel::Warning,
            format!(
                "{SCRUB_LOCK_PATH} is still held by {holder} after the {} this pass waits for \
                 setup — this scrub pass did NOT run; run `btrdasd scrub run` once setup has \
                 finished",
                wait_words(Duration::from_secs(SETUP_WAIT_SECS))
            ),
            format!("Skipped — {holder} held the scrub lock; this pass did NOT run"),
        ),
        ScrubLockAttempt::Held(holder) => {
            let named = if holder == crate::maintenance::UNKNOWN_HOLDER {
                format!("{holder} (its record is empty or unreadable)")
            } else {
                holder.clone()
            };
            (
                LogLevel::Info,
                format!("{SCRUB_LOCK_PATH} is held by {named} — skipping this invocation"),
                format!("Skipped — {named} holds the scrub lock"),
            )
        }
        // Not a skip; listed so a new variant must be decided here.
        ScrubLockAttempt::Taken(_) => (
            LogLevel::Info,
            "the scrub locks were taken".to_string(),
            "the scrub locks were taken".to_string(),
        ),
    }
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttemptState {
    /// `running`, `yielded`, `paused`, `finished`, `canceled`, `aborted`,
    /// `interrupted`, or `error`. `paused` (bd DAS-Backup-Manager-su1b.15) is
    /// a scrub ended by `btrdasd scrub pause`: resumable, never in progress,
    /// never a failure.
    pub outcome: String,
    /// Authoritative pass/fail bit for this filesystem.
    pub ok: bool,
    pub started_epoch: i64,
    pub finished_epoch: i64,
    pub duration_secs: u64,
    pub bytes_scrubbed: u64,
    pub error_total: u64,
    pub counters: ScrubCounters,
    /// `true` when `bytes_scrubbed`, `error_total` and `counters` are not
    /// measurements but placeholders: the attempt ended (interrupted,
    /// paused) without its status record being read, so the real figures are
    /// unknown (bd DAS-Backup-Manager-su1b.22). Readers show `unknown`, never
    /// 0 or "no errors". Absent in older files, which read as measured.
    #[serde(default, skip_serializing_if = "is_false")]
    pub counters_unknown: bool,
    #[serde(default)]
    pub messages: Vec<String>,
    /// The signal that ended the pass (`SIGTERM`, `SIGINT`, `SIGHUP`), when
    /// `outcome` is `interrupted` and the pass caught it. Absent for an
    /// interruption found afterwards (a `running` record nobody finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    /// Process that wrote a `running` record, to tell it from a stale one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Boot the `running` record was written in (`/proc/sys/kernel/random/boot_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// On a `yielded` record: the holder record of the claim the pass gave
    /// (on a `paused` one: who asked for the pause, bd DAS-Backup-Manager-su1b.15)
    /// the maintenance lock up for (`backup-run.sh pid 1234`), as the
    /// claimant wrote it. Display only; free text from a lock file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yielded_to: Option<String>,
    /// On a `yielded` record: when the pass gave the lock up (on a `paused`
    /// one: when the pass ended paused). Absent means
    /// unknown, never "at the epoch".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yielded_epoch: Option<i64>,
    /// On a `yielded` record: how many times this pass has yielded so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yields: Option<u32>,
    /// On a `yielded` record: total seconds this pass has spent paused
    /// before this yield.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_secs: Option<u64>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl AttemptState {
    /// The damage-counter total, `None` when the attempt's figures are
    /// unknown ([`Self::counters_unknown`]).
    pub fn error_total_known(&self) -> Option<u64> {
        (!self.counters_unknown).then_some(self.error_total)
    }

    /// The counters as text for `scrub status`: `unknown` when unread.
    pub fn counters_summary(&self) -> String {
        if self.counters_unknown {
            UNKNOWN.to_string()
        } else {
            self.counters.summary()
        }
    }

    /// Whether this record says the pass is in progress: `running`
    /// (scrubbing now) or `yielded` (paused for a backup, waiting to resume;
    /// bd DAS-Backup-Manager-su1b.2).
    pub fn is_in_progress(&self) -> bool {
        self.outcome == "running" || self.outcome == "yielded"
    }

    /// Whether this is an in-progress record ([`Self::is_in_progress`])
    /// whose pass is still alive: the process that wrote it exists and this
    /// is the boot it was written in. (A recycled pid inside one boot could
    /// fool it; the next pass replaces the record either way.)
    pub fn is_live(&self) -> bool {
        if !self.is_in_progress() {
            return false;
        }
        let (Some(pid), Some(boot)) = (self.pid, self.boot_id.as_deref()) else {
            return false;
        };
        current_boot_id().as_deref() == Some(boot) && Path::new(&format!("/proc/{pid}")).exists()
    }

    /// The outcome to show and judge by: a `running` or `yielded` record
    /// that nobody is running any more is an interrupted pass whose end was
    /// never recorded (SIGKILL, a power cut, a reboot that outran the
    /// handler, or one while the pass was paused for a backup).
    pub fn effective_outcome(&self) -> String {
        if self.is_in_progress() && !self.is_live() {
            "interrupted".to_string()
        } else {
            self.outcome.clone()
        }
    }
}

/// What a reader prints for a figure that was never measured.
pub const UNKNOWN: &str = "unknown";

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
    /// The pass `btrdasd scrub pause` last ended, while it waits for
    /// `btrdasd scrub resume` (bd DAS-Backup-Manager-su1b.24). Absent once a
    /// pass runs to its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_pass: Option<PausedPass>,
}

impl Default for ScrubState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            last_pass: None,
            filesystems: BTreeMap::new(),
            paused_pass: None,
        }
    }
}

/// A pass a pause ended, as the pass that resumes it needs it
/// (bd DAS-Backup-Manager-su1b.24): the filesystems already scrubbed to the
/// end in it are not scrubbed again. Paused again, the resuming pass carries
/// it forward, so a pass paused twice still remembers what its first part did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PausedPass {
    /// When the first pass of this chain started.
    pub origin_epoch: i64,
    /// When the pass last ended paused.
    pub paused_epoch: i64,
    /// FS UUIDs whose scrub finished in this chain (kernel `finished`,
    /// whatever it found: the findings went out with that pass's report).
    pub finished: Vec<String>,
}

/// How long after a pause the pass that starts next still continues it
/// (bd DAS-Backup-Manager-su1b.24). The pause request lives in `/run`, so a
/// reboot takes it away and the next scheduled pass may start weeks later:
/// past this a pass scrubs every filesystem again, the cautious side.
pub const RESUME_WINDOW_SECS: i64 = 7 * 86_400;

/// Whether a pass starting at `started_epoch` continues `marker`'s pass.
pub fn continues_paused_pass(marker: &PausedPass, started_epoch: i64) -> bool {
    let since = started_epoch - marker.paused_epoch;
    (0..=RESUME_WINDOW_SECS).contains(&since)
}

/// The `[scrub].targets` labels a pass starting at `started_epoch` does not
/// scrub again: those its paused pass already scrubbed to the end
/// (bd DAS-Backup-Manager-su1b.24). Each is named in the log; anything
/// doubtful (no marker, a stale one, a label whose UUID cannot be resolved)
/// is scrubbed.
pub fn labels_already_scrubbed(
    state: &ScrubState,
    config: &Config,
    started_epoch: i64,
    progress: &dyn ProgressCallback,
) -> Vec<String> {
    let Some(marker) = &state.paused_pass else {
        return Vec::new();
    };
    if !continues_paused_pass(marker, started_epoch) {
        progress.on_log(
            LogLevel::Info,
            &format!(
                "The pass paused at {} is not continued (older than {} days, or the clock \
                 went back): every filesystem is scrubbed",
                crate::caldate::local_datetime(marker.paused_epoch)
                    .unwrap_or_else(|| format!("epoch {}", marker.paused_epoch)),
                RESUME_WINDOW_SECS / 86_400
            ),
        );
        return Vec::new();
    }
    config
        .scrub_target_labels()
        .into_iter()
        .filter(|label| {
            let done = resolve_target_fsuuid(config, label)
                .is_ok_and(|uuid| marker.finished.contains(&uuid));
            if done {
                progress.on_log(
                    LogLevel::Info,
                    &format!(
                        "{label}: already scrubbed to the end in the pass paused at {} — \
                         not scrubbed again",
                        crate::caldate::local_datetime(marker.paused_epoch)
                            .unwrap_or_else(|| format!("epoch {}", marker.paused_epoch))
                    ),
                );
            }
            done
        })
        .collect()
}

/// Path of the state file (overridable for tests via `DAS_SCRUB_STATE`).
pub fn state_path() -> PathBuf {
    overridable("DAS_SCRUB_STATE", SCRUB_STATE_PATH)
}

/// Load the state file. A missing file yields the default (empty) state.
///
/// **Pure**: this never writes, renames, or deletes anything, so the
/// unprivileged readers (health checks, the GUI) can call it freely and get an
/// error that means exactly one thing. Unparsable JSON is reported as
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

/// Move an unparsable state file aside to `<path>.corrupt`, preserving the
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
    for result in &pass.results {
        merge_result_into_state(state, result, pass.finished_epoch);
    }
    state.paused_pass = next_paused_pass(state.paused_pass.take(), pass);
    state.last_pass = Some(PassSummary {
        started_epoch: pass.started_epoch,
        finished_epoch: pass.finished_epoch,
        targets_attempted: pass.results.len(),
        targets_failed: pass.failed_count(),
        success: pass.success(),
    });
}

/// The paused-pass marker after `pass` (bd DAS-Backup-Manager-su1b.24): a
/// pass that ends paused files the filesystems it scrubbed to the end, on top
/// of the marker it continued; any other pass leaves none.
fn next_paused_pass(prior: Option<PausedPass>, pass: &ScrubPass) -> Option<PausedPass> {
    if pass.status != PassStatus::Paused {
        return None;
    }
    let paused_epoch = pass
        .results
        .iter()
        .find_map(|r| r.paused.as_ref().map(|p| p.at_epoch))
        .unwrap_or(pass.finished_epoch);
    let (origin_epoch, mut finished) = match prior {
        Some(m) if continues_paused_pass(&m, pass.started_epoch) => (m.origin_epoch, m.finished),
        _ => (pass.started_epoch, Vec::new()),
    };
    for r in &pass.results {
        let scrubbed = r.paused.is_none()
            && r.interrupted_by.is_none()
            && r.outcome == Some(ScrubOutcome::Finished)
            && !r.fsuuid.is_empty();
        if scrubbed && !finished.contains(&r.fsuuid) {
            finished.push(r.fsuuid.clone());
        }
    }
    Some(PausedPass {
        origin_epoch,
        paused_epoch,
        finished,
    })
}

/// Fold one filesystem's result into `state`. `pass_finished_epoch` stands in
/// for a result that carries no finish time of its own.
fn merge_result_into_state(
    state: &mut ScrubState,
    result: &ScrubFsResult,
    pass_finished_epoch: i64,
) {
    state.schema_version = STATE_SCHEMA_VERSION;
    {
        if result.fsuuid.is_empty() {
            // No UUID resolved — there is no stable key to file this under, so
            // it stays in the email/report only.
            return;
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
                pass_finished_epoch
            })
        } else {
            prior_success
        };
        // Placeholders are filed as such: unknown, never a measured zero.
        let counters_unknown = result.counters_unknown();
        let counters = if counters_unknown {
            ScrubCounters::default()
        } else {
            result.counters
        };
        let bytes_scrubbed = if counters_unknown {
            0
        } else {
            result.bytes_scrubbed
        };
        state.filesystems.insert(
            result.fsuuid.clone(),
            FsState {
                target_label: result.target_label.clone(),
                mountpoint: result.mountpoint.clone(),
                last_success_epoch,
                last_attempt: AttemptState {
                    outcome: match (&result.paused, result.interrupted_by, result.outcome) {
                        (Some(_), _, _) => "paused".to_string(),
                        (None, Some(_), _) => "interrupted".to_string(),
                        (None, None, Some(o)) => o.as_str().to_string(),
                        (None, None, None) => "error".to_string(),
                    },
                    yielded_to: result.paused.as_ref().map(|p| p.by.clone()),
                    yielded_epoch: result.paused.as_ref().map(|p| p.at_epoch),
                    yields: result.paused.as_ref().map(|_| result.yields.len() as u32),
                    signal: result.interrupted_by.map(signal_name),
                    ok,
                    started_epoch: result.started_epoch,
                    finished_epoch: result.finished_epoch,
                    duration_secs: result.duration_secs,
                    bytes_scrubbed,
                    error_total: counters.error_total(),
                    counters,
                    counters_unknown,
                    messages: result.errors.clone(),
                    ..Default::default()
                },
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Pass results
// ---------------------------------------------------------------------------

/// Whether the pass ran or stepped aside for another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassStatus {
    /// The pass ran (individual filesystems may still have failed).
    Completed,
    /// Another scrub held the singleton lock, or `btrdasd setup` still held
    /// it after the pass waited [`SETUP_WAIT_SECS`] — nothing was scrubbed,
    /// and no state was written.
    Skipped,
    /// The scrub is paused by hand (bd DAS-Backup-Manager-su1b.15): a pause
    /// request ended the pass, its filesystem recorded `paused`, or the pass
    /// started while one was in place and did not run. Not a failure.
    Paused,
}

/// A filesystem's scrub ended by `btrdasd scrub pause` (bd
/// DAS-Backup-Manager-su1b.15): resumable, never a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PausedBy {
    /// Who asked, as the pause request names it.
    pub by: String,
    /// When the pass ended paused.
    pub at_epoch: i64,
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
    /// actually succeeded, never set preemptively.
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
    /// The signal that stopped this filesystem's scrub (or kept it from
    /// starting), when the pass caught one.
    pub interrupted_by: Option<i32>,
    /// Each time this filesystem's scrub gave the maintenance lock up to a
    /// waiting backup and later resumed (bd DAS-Backup-Manager-su1b.2).
    /// Information for the report, never an error: a target that yielded
    /// and then finished clean is a clean pass.
    pub yields: Vec<ScrubYield>,
    /// Set by a segment of this filesystem's scrub that stopped for a
    /// waiting backup (bd DAS-Backup-Manager-su1b.2, step A5): the pass gives
    /// the maintenance lock up next. Never persisted; the pass loop takes it.
    pub pending_yield: Option<PendingYield>,
    /// Set when a pause request ended the pass while this filesystem's scrub
    /// was stopped for it: filed `paused`, not counted as a failure.
    pub paused: Option<PausedBy>,
    /// Set when an interrupted filesystem's status record was read and
    /// belongs to this run, so `counters` and `bytes_scrubbed` are its
    /// figures as of the interruption (bd DAS-Backup-Manager-su1b.22).
    pub interruption_record_read: bool,
}

/// What a scrub segment that stopped for a waiting backup asks of the pass
/// loop, its unmount already attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingYield {
    /// Resume this filesystem once the lock is back (`false`: the kernel was
    /// not seen to stop its scrub, so the mount was left and this filesystem
    /// is not scrubbed again in this pass).
    pub resume: bool,
}

/// One pause of a filesystem's scrub for a backup, as the report lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubYield {
    /// When the scrub gave the maintenance lock up.
    pub at_epoch: i64,
    /// The claim's holder record (`backup-run.sh pid 1234`), as written.
    pub holder: String,
    /// How long the scrub waited before it took the lock again; `None`
    /// while unknown (the pass ended before it resumed).
    pub paused_secs: Option<u64>,
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
            interrupted_by: None,
            yields: Vec::new(),
            pending_yield: None,
            paused: None,
            interruption_record_read: false,
        }
    }

    /// Whether `counters` and `bytes_scrubbed` are placeholders rather than
    /// measurements: an interrupted or paused filesystem whose status record
    /// was not read (bd DAS-Backup-Manager-su1b.22). A zero standing in for
    /// an unread measurement is a defect; reports say `unknown` instead.
    pub fn counters_unknown(&self) -> bool {
        // A result read from a record (`outcome` set) is measured, even when a
        // signal was attached to it afterwards.
        self.outcome.is_none()
            && (self.paused.is_some()
                || (self.interrupted_by.is_some() && !self.interruption_record_read))
    }

    /// The counters as text for reports: `unknown` when unread.
    pub fn counters_summary(&self) -> String {
        if self.counters_unknown() {
            UNKNOWN.to_string()
        } else {
            self.counters.summary()
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
        } else if self.paused.is_some() {
            "PAUSED"
        } else if self.interrupted_by.is_some() {
            "INTERRUPTED"
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
    /// Labels not scrubbed because the paused pass this one resumes already
    /// scrubbed them to the end (bd DAS-Backup-Manager-su1b.24). Not results:
    /// their finished records stand as that pass filed them.
    pub already_scrubbed: Vec<String>,
}

impl ScrubPass {
    /// Number of filesystems that did not pass.
    pub fn failed_count(&self) -> usize {
        // A filesystem paused by hand is not a failure (bd DAS-Backup-Manager-su1b.15).
        self.results
            .iter()
            .filter(|r| !r.ok() && r.paused.is_none())
            .count()
    }

    /// How many filesystems scrubbed cleanly: a paused one is neither clean
    /// nor failed (bd DAS-Backup-Manager-su1b.15).
    pub fn clean_count(&self) -> usize {
        self.results.iter().filter(|r| r.ok()).count()
    }

    /// Whether every scrubbed filesystem passed and the pass hit no
    /// pass-level errors. A skipped pass counts as a success (nothing broke).
    pub fn success(&self) -> bool {
        self.errors.is_empty() && self.failed_count() == 0
    }

    /// The signal that stopped the pass, if one did.
    pub fn interrupted_by(&self) -> Option<i32> {
        self.results.iter().find_map(|r| r.interrupted_by)
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

/// What the next scrub invocation should do, given the prior record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrubStartMode {
    /// Plain `btrfs scrub start -B` — no interrupted pass to continue.
    Normal,
    /// `btrfs scrub resume -B` (every scrub argv also carries the priority flags
    /// of `ScrubIoClass`), because a previous pass left an aborted record
    /// behind (interrupted by a reboot or an unmount) and nothing is running
    /// now. Resuming continues from the saved position in
    /// `/var/lib/btrfs/scrub.status.<uuid>` rather than re-scrubbing from zero,
    /// so an interruption is actually made good instead of silently discarded
    /// (bd DAS-Backup-Manager-292). If the kernel has no resumable state after
    /// all — `btrfs scrub resume` prints "nothing to resume" — the runner falls
    /// back to a fresh `start -B -f`, which is what this branch did before
    /// resume-awareness and still clears a genuinely stale record.
    Resume { reason: String },
}

/// Decide whether this filesystem's next scrub should resume or start fresh.
///
/// An [`ScrubOutcome::Aborted`] record (`canceled:0 finished:0`) is what
/// `btrfs scrub status` leaves behind when a pass is interrupted by a reboot or
/// — the DAS case — an unmount, and it is exactly the state in which a
/// resumable position exists in `/var/lib/btrfs/scrub.status.<uuid>`. Proven on
/// 2026-09-09 (bd DAS-Backup-Manager-292): a scrub cancelled and then carried
/// through an unmount/remount cycle resumed from 997 MB and continued to
/// 9.58 GB, and the status record survived the unmount because it lives on the
/// host root keyed by FS UUID, not on the target. So the interrupted case
/// returns [`ScrubStartMode::Resume`] rather than re-scrubbing from zero.
///
/// This decision is deliberately kept a pure function of the on-disk record and
/// the live kernel state — the `--decide` seam the sentinel wrapper uses — so
/// both directions are testable against real filesystem states rather than
/// fixtures. The three guards on returning `Resume` are load-bearing:
/// - **only when the prior record is `Aborted`** — a `Finished` or `Canceled`
///   record has no interrupted position to continue, and a deliberate operator
///   cancel is not silently resumed;
/// - **only when the kernel confirms nothing is running** — the locks here do
///   not protect against an operator's hand-run `btrfs scrub`, and resuming
///   into a live scrub is meaningless;
/// - **`Unknown` live state falls back to `Normal`** — a plain start fails
///   loudly rather than acting on a state we could not read.
///
/// Note this never returns a force-start: the `-f` that a genuinely stale
/// record needs is applied by the runner only as a fallback, if `resume`
/// reports "nothing to resume". That keeps the stale-record-clearing capability
/// this branch always had while making position-preservation the default.
pub fn decide_scrub_start_mode(fsuuid: &str, mount_point: &str) -> ScrubStartMode {
    let Ok(prior) = read_scrub_status(fsuuid) else {
        // No prior record at all — nothing to resume.
        return ScrubStartMode::Normal;
    };
    // The decision itself is factored into the pure `decide_from` so both
    // directions (resume vs normal) are unit-testable without a real mount —
    // the live state a real filesystem yields only `Unknown` off a fake path,
    // which cannot distinguish the branches. This wrapper does only the I/O:
    // read the record, probe the kernel, hand both to the pure decision.
    decide_from(
        fsuuid,
        prior.outcome(),
        live_scrub_state(mount_point, fsuuid),
    )
}

/// Pure resume-or-normal decision from the two facts that determine it: the
/// prior record's outcome and whether a scrub is live now. Kept free of I/O so
/// every combination is exercisable in a unit test — the `--decide` seam.
fn decide_from(fsuuid: &str, prior: ScrubOutcome, live: LiveScrubState) -> ScrubStartMode {
    // Only an *aborted* record carries a resumable position; a finished or
    // deliberately-canceled one does not.
    if prior != ScrubOutcome::Aborted {
        return ScrubStartMode::Normal;
    }
    match live {
        LiveScrubState::NotRunning => ScrubStartMode::Resume {
            reason: format!(
                "previous scrub of {fsuuid} left an aborted record \
                 (canceled:0 finished:0) and the kernel reports no scrub running — \
                 resuming from the saved position instead of restarting from zero"
            ),
        },
        // A scrub is already running, or liveness could not be read: never act
        // on a guess — a plain start fails loudly rather than stomping.
        LiveScrubState::Running | LiveScrubState::Unknown => ScrubStartMode::Normal,
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
///
/// The report is mailed through [`crate::report::MAILX`]: no caller can hand
/// a pass another mailer (bd DAS-Backup-Manager-1x76); `email_scrub_report`
/// is what tests drive.
/// `reload` reads `config.toml` again: once the locks are held (a wait for
/// setup or for the maintenance lock may have outlived the file the caller
/// read), and after every pause for a backup, before anything is mounted
/// (bd DAS-Backup-Manager-su1b.2).
pub fn run_scrub_pass(
    config: &Config,
    reload: &dyn Fn() -> Result<Config, String>,
    progress: &dyn ProgressCallback,
) -> Result<ScrubPass, ScrubError> {
    if config.scrub_target_labels().is_empty() {
        return Err(ScrubError::NoTargets);
    }

    let started_epoch = now_epoch();

    if let Some(pass) = paused_at_start(&pause_request_path(), started_epoch, progress) {
        return Ok(pass);
    }

    let attempt = acquire_locks(progress)?;
    let ScrubLockAttempt::Taken(mut locks) = attempt else {
        let (level, line, summary) = skipped_lines(&attempt);
        progress.on_log(level, &line);
        // Close the progress lifecycle on this path too, so a caller driving a
        // UI or a log does not see a stage opened and never completed.
        progress.on_complete(true, &summary);
        return Ok(ScrubPass {
            status: PassStatus::Skipped,
            started_epoch,
            finished_epoch: now_epoch(),
            results: Vec::new(),
            errors: Vec::new(),
            already_scrubbed: Vec::new(),
        });
    };

    // The file may have changed while this pass waited for its locks.
    let config = &reload().map_err(|detail| ScrubError::ConfigReload { detail })?;
    if config.scrub_target_labels().is_empty() {
        return Err(ScrubError::NoTargets);
    }

    if !config.scrub.enabled {
        progress.on_log(
            LogLevel::Warning,
            "[scrub].enabled is false in config — running anyway (manual/forced invocation)",
        );
    }

    // A stop request (SIGTERM from a reboot or `systemctl stop`, Ctrl-C) is
    // caught for the rest of the pass: the scrub is stopped, the stop is
    // recorded, the filesystem is unmounted; the caller then dies of the signal.
    let _termination = catch_termination();
    let claim = ClaimWatch::new(&claim_lock_path()).with_pause_request(&pause_request_path());
    let state = state_path();
    // A state file that cannot be read skips nothing: every target is scrubbed.
    let already_scrubbed = load_state_from(&state)
        .map(|s| labels_already_scrubbed(&s, config, started_epoch, progress))
        .unwrap_or_default();
    let results = scrub_targets_yielding(
        config,
        progress,
        &mut locks.maintenance,
        &claim,
        &already_scrubbed,
        &PassHooks {
            stop: &pending_signal,
            reload,
            scrub_one: &scrub_one_target,
            file_result: &|result| record_result_logged(&state, result, progress),
            file_yield: &|mark| record_yielded_logged(&state, mark, progress),
        },
    );

    let paused = results.iter().any(|r| r.paused.is_some());
    if !paused {
        lift_unmet_pause_request(&pause_request_path(), progress);
    }
    let mut pass = ScrubPass {
        status: if paused {
            PassStatus::Paused
        } else {
            PassStatus::Completed
        },
        started_epoch,
        finished_epoch: now_epoch(),
        results,
        errors: Vec::new(),
        already_scrubbed,
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

    email_scrub_report(&pass, config, progress, &crate::report::MAILX);

    let summary = match pass.interrupted_by() {
        Some(signal) => format!(
            "Interrupted by {} — {} of {} filesystems scrubbed cleanly before it",
            signal_name(signal),
            pass.clean_count(),
            pass.results.len()
        ),
        None => format!(
            "{} of {} filesystems scrubbed cleanly",
            pass.clean_count(),
            pass.results.len()
        ),
    };
    progress.on_complete(pass.success(), &summary);

    Ok(pass)
}

/// Mail the pass's report through `mailer`. A report that cannot be sent is
/// a warning: the pass's state is already persisted, and the pass stands.
fn email_scrub_report(
    pass: &ScrubPass,
    config: &Config,
    progress: &dyn ProgressCallback,
    mailer: &dyn crate::report::Mailer,
) {
    let report = format_scrub_report(pass, config);
    match crate::report::send_email_report_with_kind(&report, config, "Scrub", mailer) {
        Ok(()) => progress.on_log(LogLevel::Info, "Scrub report emailed"),
        Err(e) => progress.on_log(
            LogLevel::Warning,
            &format!("Could not send scrub report email: {e}"),
        ),
    }
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
    let mut state = load_state_for_write(&path, progress)?;
    merge_pass_into_state(&mut state, pass);
    save_state_to(&state, &path)?;
    Ok(path)
}

/// Load the state a writer is about to change: an unparsable file is
/// quarantined and a fresh state started (said in the log); any other read
/// failure is returned, so an intact file is never overwritten from blank.
fn load_state_for_write(
    path: &Path,
    progress: &dyn ProgressCallback,
) -> Result<ScrubState, ScrubError> {
    match load_state_from(path) {
        Ok(state) => Ok(state),
        Err(ScrubError::StateCorrupt { detail, .. }) => {
            let moved = quarantine_state_file(path)?;
            progress.on_log(
                LogLevel::Warning,
                &format!(
                    "Scrub state file was unparsable ({detail}) — quarantined to {} and rebuilt",
                    moved.display()
                ),
            );
            Ok(ScrubState::default())
        }
        Err(e) => Err(e),
    }
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
                result.counters_summary()
            )
        }
        Some(ScrubOutcome::Canceled) => {
            format!("scrub canceled; {}", result.counters_summary())
        }
        Some(ScrubOutcome::Finished) => {
            format!("errors found: {}", result.counters_summary())
        }
        None => "no scrub status record produced".to_string(),
    }
}

/// Mount, scrub, parse, unmount — one filesystem, under the maintenance lock
/// `held` proves.
fn scrub_one_target(
    config: &Config,
    label: &str,
    progress: &dyn ProgressCallback,
    held: &MaintenanceHeld,
    claimed: &dyn Fn() -> bool,
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
    // A RAID-1 target is mounted only through the guard (bd
    // DAS-Backup-Manager-su1b.10): never `degraded` as configured.
    let guarded = crate::raid_guard::is_guarded(target).then_some((config, target));
    // Whether the guard mounted it normally: only such a mount gets the
    // end-of-pass check (a degraded one shows its absent member missing all
    // its life, its period recorded at the mount; bd su1b.14, review note 1).
    let mut guard_normal = false;
    match ensure_mounted(
        &mount_point,
        &fsuuid,
        &config.das.mount_opts,
        guarded,
        &mut guard_normal,
        progress,
        held,
    ) {
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
    // `ScrubFsResult::scrub_launched` for why a preemptive stamp here was a
    // bug (`bd DAS-Backup-Manager-18p` review).
    let scrub_started = now_epoch();
    record_running_logged(
        &state_path(),
        &RunningMarker {
            label,
            fsuuid: &fsuuid,
            mountpoint: &mount_point,
            started_epoch: scrub_started,
        },
        progress,
    );
    let mut yielded = None;
    match run_btrfs_scrub(&mount_point, &fsuuid, progress, claimed) {
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
        ScrubRunOutcome::Interrupted(signal) => {
            result.started_epoch = scrub_started;
            result.interrupted_by = Some(signal);
            result
                .errors
                .push(format!("scrub interrupted by {}", signal_name(signal)));
        }
        ScrubRunOutcome::Yielded { launched } => {
            if launched {
                result.started_epoch = scrub_started;
                result.scrub_launched = true;
            }
            yielded = Some(launched);
        }
    }

    // --- the yield (bd DAS-Backup-Manager-su1b.2) -------------------------
    // The child is reaped. Before the mount goes: did the kernel stop, and
    // what did the segment leave? Nothing is read through the mount point
    // unless it is verified to be this filesystem (`live_scrub_state`).
    if let Some(launched) = yielded {
        let (live, record) = if launched {
            (
                live_scrub_state(&mount_point, &fsuuid),
                read_scrub_status(&fsuuid).ok(),
            )
        } else {
            (LiveScrubState::NotRunning, None)
        };
        match yield_verdict(launched, live, record.as_ref(), scrub_started) {
            YieldVerdict::Finished => {
                // Finished as it was stopped: read as a result below, and
                // the pass yields before the next filesystem instead.
            }
            YieldVerdict::Pause { note } => {
                if let Some(note) = note {
                    progress.on_log(LogLevel::Warning, &format!("{label}: {note}"));
                }
                if result.mounted_by_engine
                    && guard_normal
                    && let Some((config, target)) = guarded
                {
                    raid_end_check_logged(config, target, label, progress);
                }
                if result.mounted_by_engine
                    && let Err(e) = unmount(&mount_point, progress)
                {
                    // A-F10: the backup's leftover check takes it down, or
                    // refuses; recorded either way.
                    result
                        .errors
                        .push(format!("{e} (when pausing the scrub for a backup)"));
                }
                result.pending_yield = Some(PendingYield { resume: true });
                return result;
            }
            YieldVerdict::KernelLive(why) => {
                result.errors.push(why);
                result.pending_yield = Some(PendingYield { resume: false });
                return result;
            }
        }
    }

    // --- status by UUID (never by path) -----------------------------------
    // An interrupted scrub's record is whatever the stopped child left, not a
    // result: the interruption is the result.
    if result.interrupted_by.is_some() {
        take_interrupted_figures(&mut result, read_scrub_status(&fsuuid), scrub_started);
    } else {
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
    }

    if result.finished_epoch == 0 {
        result.finished_epoch = now_epoch();
    }

    // --- a member that dropped during the pass (bd DAS-Backup-Manager-su1b.14)
    // Recorded before anything is cleared or unmounted: a scrub that lost a
    // member pays no debt, and the next mount with both owes one again.
    if result.mounted_by_engine
        && guard_normal
        && let Some((config, target)) = guarded
    {
        raid_end_check_logged(config, target, label, progress);
    }

    // --- a scrub owed after a degraded period (bd DAS-Backup-Manager-su1b.10)
    // A completed scrub with nothing uncorrectable, begun after every member
    // was present again, pays it: both record copies go, while the
    // filesystem is still mounted (the witness is on it). Corrected errors
    // are expected here — they are the stale copies being repaired.
    if guarded.is_some()
        && result.errors.is_empty()
        && result.interrupted_by.is_none()
        && result.outcome == Some(ScrubOutcome::Finished)
        && result.counters.uncorrectable_errors == 0
    {
        match crate::raid_guard::clear_after_scrub(
            &crate::raid_guard::record_dir(),
            &mount_point,
            &fsuuid,
            result.started_epoch,
            crate::raid_guard::missing_devids(&fsuuid),
        ) {
            Ok(Some(_)) => progress.on_log(
                LogLevel::Info,
                &format!(
                    "{label}: the scrub owed after its degraded period is done — RAID record \
                     cleared; the pair is trusted again"
                ),
            ),
            Ok(None) => {}
            Err(e) => progress.on_log(
                LogLevel::Warning,
                &format!("{label}: the RAID record was not cleared: {e}"),
            ),
        }
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

/// Take an interrupted filesystem's figures from its btrfs status record
/// (bd DAS-Backup-Manager-su1b.22).
///
/// An interrupted scrub's record is whatever the stopped child left, not a
/// result, but its counters are real measurements as of the interruption
/// (a RAID-1 pass once left 468272 corrected errors behind a filed 0). The
/// record counts only when it belongs to this filesystem's run (the same
/// freshness anchor as a finished one); otherwise, or when it cannot be
/// read, the figures stay unknown ([`ScrubFsResult::counters_unknown`]).
fn take_interrupted_figures(
    result: &mut ScrubFsResult,
    record: Result<ScrubStatusRecord, ScrubError>,
    scrub_started: i64,
) {
    let Ok(record) = record else { return };
    let activity = record.last_activity_start();
    if activity == 0 || activity < scrub_started - RECORD_FRESHNESS_SLACK_SECS {
        return;
    }
    result.counters = record.counters();
    result.bytes_scrubbed = record.bytes_scrubbed();
    result.interruption_record_read = true;
}

/// Ensure `mount_point` holds the filesystem with `fsuuid`.
///
/// Returns whether this call performed the mount (and therefore owns the
/// unmount). The expected filesystem already mounted is reused only under a
/// handed-down hold; otherwise it was left by an earlier job and is taken
/// down and mounted afresh (bd DAS-Backup-Manager-jgm).
/// Any filesystem found at the mount point that is *not* the expected one is
/// an error — never an invitation to unmount it. `held` is the proof
/// that this process holds the maintenance lock, as for
/// `mount::ensure_targets_mounted` (bd DAS-Backup-Manager-frb).
fn ensure_mounted(
    mount_point: &str,
    fsuuid: &str,
    mount_opts: &str,
    guarded: Option<(&Config, &crate::config::Target)>,
    guard_normal: &mut bool,
    progress: &dyn ProgressCallback,
    held: &MaintenanceHeld,
) -> Result<bool, String> {
    let path = Path::new(mount_point);

    if path.exists() && crate::health::is_mountpoint(path) {
        match mounted_fs_uuid(mount_point) {
            // The target, left mounted by an earlier job: taken down and
            // mounted afresh below, so this pass owns the unmount; one that
            // will not come down, even with `umount -f`, is not scrubbed
            // (bd DAS-Backup-Manager-jgm, operator 2026-11-09).
            // Under a handed-down hold the parent mounted it for this pass
            // and takes it down itself.
            Some(found) if found.eq_ignore_ascii_case(fsuuid) && held.is_delegated() => {
                progress.on_log(
                    LogLevel::Info,
                    &format!("{mount_point} already holds {fsuuid} — reusing existing mount"),
                );
                return Ok(false);
            }
            Some(found) if found.eq_ignore_ascii_case(fsuuid) => {
                crate::mount::take_down_leftover(mount_point, progress).map_err(|why| {
                    format!(
                        "{mount_point} holds {fsuuid}, left mounted by an earlier job, and \
                         cannot be unmounted ({why}) — refusing to scrub until it is"
                    )
                })?;
                progress.on_log(
                    LogLevel::Warning,
                    &format!(
                        "{mount_point} holds {fsuuid}, left mounted by an earlier job — \
                         unmounted, to be mounted afresh for this pass"
                    ),
                );
            }
            Some(found) => {
                return Err(format!(
                    "{mount_point} is mounted but holds filesystem '{found}', expected \
                     '{fsuuid}' — refusing to scrub the wrong filesystem"
                ));
            }
            None => {
                return Err(format!(
                    "{mount_point} is a mountpoint but findmnt could not report its filesystem \
                     UUID"
                ));
            }
        }
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

    if let Some((config, target)) = guarded {
        match crate::mount::mount_guarded_target(
            config,
            target,
            crate::raid_guard::Job::Scrub.intent(),
            progress,
            held,
        ) {
            Ok(outcome) => {
                *guard_normal = matches!(outcome, crate::raid_guard::Outcome::Normal { .. });
            }
            Err(crate::raid_guard::Failure::Refused { why, left_mounted }) => {
                if !left_mounted {
                    cleanup_dir(progress);
                }
                return Err(format!(
                    "RAID-1 mount of {fsuuid} refused: {why}{}",
                    if left_mounted {
                        format!(" ({mount_point} is still mounted)")
                    } else {
                        String::new()
                    }
                ));
            }
            Err(other) => {
                cleanup_dir(progress);
                return Err(format!(
                    "RAID-1 mount of {fsuuid} → {mount_point} failed: {}",
                    other.why()
                ));
            }
        }
        return verify_engine_mount(mount_point, fsuuid, progress, cleanup_dir);
    }

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

    verify_engine_mount(mount_point, fsuuid, progress, cleanup_dir)
}

/// Verify what actually landed at `mount_point` after the engine mounted it.
/// A successful mount of the wrong filesystem is the exact class of bug this
/// module exists to prevent.
fn verify_engine_mount(
    mount_point: &str,
    fsuuid: &str,
    progress: &dyn ProgressCallback,
    cleanup_dir: impl Fn(&dyn ProgressCallback),
) -> Result<bool, String> {
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
/// The end-of-job check of a RAID-1 target the pass mounted, said: a recorded
/// drop or a problem as a warning (bd DAS-Backup-Manager-su1b.14).
fn raid_end_check_logged(
    config: &Config,
    target: &crate::config::Target,
    label: &str,
    progress: &dyn ProgressCallback,
) {
    let check = crate::mount::raid_end_check(config, target);
    let level = if matches!(check, crate::raid_guard::EndCheck::Whole) {
        LogLevel::Info
    } else {
        LogLevel::Warning
    };
    progress.on_log(
        level,
        &format!("{label}: {}", crate::raid_guard::end_check_line(&check)),
    );
}

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
    /// The pass caught a termination signal while the child ran (or before it
    /// started); the child was stopped and reaped first.
    Interrupted(i32),
    /// A backup claimed the maintenance lock (bd DAS-Backup-Manager-su1b.2):
    /// the child, if `launched`, was stopped by [`stop_child`] and reaped, so
    /// its record stays resumable; not `launched`, none was started.
    Yielded { launched: bool },
}

/// True when a `resume` attempt exited nonzero *specifically* because the
/// kernel had no resumable position — `btrfs scrub resume` prints
/// "nothing to resume for <mnt>" and exits 2 (observed 2026-09-09, bd
/// DAS-Backup-Manager-292). Only this exact case is retried as a fresh start;
/// any other launched error is a real failure and is returned as-is, so a
/// broken scrub is never masked by silently restarting from zero.
fn resume_found_nothing(outcome: &ScrubRunOutcome) -> bool {
    matches!(
        outcome,
        ScrubRunOutcome::Launched(Err(msg)) if msg.contains("nothing to resume")
    )
}

/// The I/O class every `btrfs scrub` runs at, passed as `-c`/`-n` and chosen
/// per filesystem just before each `start` / `resume` / `start -f` by
/// [`choose_scrub_io_class`] (operator, 2026-11-22, bd
/// DAS-Backup-Manager-su1b.5; before it, always best-effort, bd
/// DAS-Backup-Manager-su1b.1).
///
/// Why on the argv and not only in the unit: btrfs-progs (v7.3,
/// `cmds/scrub.c`) defaults the class to *idle* and each per-device worker
/// thread calls `ioprio_set(IOPRIO_WHO_PROCESS, 0, ..)` unconditionally before
/// `BTRFS_IOC_SCRUB`, so the workers never inherit the unit's `Nice=` or
/// `IOSchedulingClass=`; only `-c`/`-n` change them. `scrub resume` accepts
/// the same flags.
///
/// Real-time (class 1) only where its starvation is bounded: mq-deadline with
/// I/O-priority aging (`queue/iosched/prio_aging_expire`, 10 000 ms by
/// default) dispatches a lower class's request once it has waited that long,
/// so real-time cannot hold writes back for longer. BFQ has no such bound:
/// there real-time preempts everything, writes included, so a member device
/// on any other scheduler, or anything unreadable, keeps best-effort level 0,
/// the highest class that cannot block writes under any scheduler.
///
/// DEPRECATED UPSTREAM, chosen anyway: `man btrfs-scrub` (v7.3) lists `-c`/`-n`
/// under "Deprecated options" because ioprio only works with some I/O
/// schedulers, naming mq-deadline as one it does not work with; that is
/// outdated (mq-deadline honours I/O classes and exposes `prio_aging_expire`,
/// kernel 7.3.5, 2026-11-22). The per-worker `ioprio_set` leaves no other way
/// to lift the workers. If a later btrfs-progs drops the flags every
/// `scrub start`/`resume` exits nonzero on an unrecognised option; the pass
/// still exits 0, so the weekly doctor checks they are still listed (bd
/// `DAS-Backup-Manager-su1b.3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrubIoClass {
    /// `-c 1 -n 0`: real-time, level 0.
    Realtime,
    /// `-c 2 -n 0`: best-effort, level 0.
    BestEffort,
}

impl ScrubIoClass {
    /// The `-c`/`-n` flags for this class.
    fn args(self) -> [&'static str; 4] {
        match self {
            ScrubIoClass::Realtime => ["-c", "1", "-n", "0"],
            ScrubIoClass::BestEffort => ["-c", "2", "-n", "0"],
        }
    }
}

/// The class [`choose_scrub_io_class`] picked for one filesystem, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrubIoChoice {
    class: ScrubIoClass,
    why: String,
}

impl ScrubIoChoice {
    fn best_effort(why: String) -> Self {
        ScrubIoChoice {
            class: ScrubIoClass::BestEffort,
            why,
        }
    }

    /// The one log line naming the class and the reason.
    fn log_line(&self, mount_point: &str) -> String {
        let class = match self.class {
            ScrubIoClass::Realtime => "realtime I/O, level 0 (-c 1 -n 0)",
            ScrubIoClass::BestEffort => "best-effort I/O, level 0 (-c 2 -n 0)",
        };
        format!("Scrub priority on {mount_point}: {class}: {}", self.why)
    }
}

/// Where sysfs is mounted; tests hand [`choose_scrub_io_class`] a fake tree.
const SYSFS_ROOT: &str = "/sys";

/// Choose the scrub's I/O class for the filesystem `fsuuid` from the sysfs
/// tree at `sysfs`: real-time only when every member device's disk runs
/// mq-deadline and exposes `queue/iosched/prio_aging_expire`; best-effort for
/// anything else, an unreadable or unexpected tree included (the cautious
/// default, `fail-silent.md`).
fn choose_scrub_io_class(sysfs: &Path, fsuuid: &str) -> ScrubIoChoice {
    match realtime_bound(sysfs, fsuuid) {
        Ok((disks, bound_ms)) => ScrubIoChoice {
            class: ScrubIoClass::Realtime,
            why: format!(
                "every device runs mq-deadline ({}; starvation bounded at {})",
                disks.join(", "),
                if bound_ms % 1000 == 0 {
                    format!("{} s", bound_ms / 1000)
                } else {
                    format!("{bound_ms} ms")
                }
            ),
        },
        Err(why) => ScrubIoChoice::best_effort(why),
    }
}

/// The member disks of `fsuuid` (sorted, each once) and the longest
/// `prio_aging_expire` among them in ms, when every one runs mq-deadline with
/// aging; otherwise the first reason it cannot be real-time.
fn realtime_bound(sysfs: &Path, fsuuid: &str) -> Result<(Vec<String>, u64), String> {
    if fsuuid.is_empty() || !fsuuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(format!("not a filesystem UUID: {fsuuid:?}"));
    }
    let dir = sysfs.join("fs/btrfs").join(fsuuid).join("devices");
    let entries = fs::read_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    if names.is_empty() {
        return Err(format!("no member device listed in {}", dir.display()));
    }
    let mut disks: Vec<String> = Vec::new();
    let mut bound_ms = 0;
    for name in &names {
        let real =
            fs::canonicalize(dir.join(name)).map_err(|e| format!("{name}: cannot resolve: {e}"))?;
        let is_partition = real
            .join("partition")
            .try_exists()
            .map_err(|e| format!("{name}: cannot read: {e}"))?;
        let disk_dir = if is_partition {
            real.parent()
                .ok_or_else(|| format!("{name}: partition without a disk"))?
        } else {
            real.as_path()
        };
        let disk = disk_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{name}: no disk name"))?;
        let queue = disk_dir.join("queue");
        let raw = fs::read_to_string(queue.join("scheduler"))
            .map_err(|e| format!("{disk}: cannot read queue/scheduler: {e}"))?;
        let active = raw
            .split_whitespace()
            .find_map(|w| w.strip_prefix('[').and_then(|w| w.strip_suffix(']')))
            .ok_or_else(|| format!("{disk}: no active scheduler in {:?}", raw.trim()))?;
        if active != "mq-deadline" {
            return Err(format!("{disk} runs {active}"));
        }
        let aging = match fs::read_to_string(queue.join("iosched/prio_aging_expire")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "{disk} runs mq-deadline without prio_aging_expire (no bound on starvation)"
                ));
            }
            Err(e) => return Err(format!("{disk}: cannot read prio_aging_expire: {e}")),
        };
        let ms = aging
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .ok_or_else(|| format!("{disk}: prio_aging_expire {:?} is no bound", aging.trim()))?;
        bound_ms = bound_ms.max(ms);
        if !disks.contains(&disk) {
            disks.push(disk);
        }
    }
    disks.sort();
    Ok((disks, bound_ms))
}

/// Which `btrfs scrub` invocation to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrubInvocation {
    /// `scrub start`: a fresh pass.
    Start,
    /// `scrub resume`: continue an interrupted pass.
    Resume,
    /// `scrub start -f`: a fresh pass that clears a stale aborted record.
    ForcedStart,
}

/// The `btrfs` argument vector for `invocation` on `mount_point`, in the
/// foreground (`-B`), carrying the `-c`/`-n` flags of `class`. The mountpoint
/// is last: [`run_scrub_argv`] recovers it from there.
fn btrfs_scrub_args(
    invocation: ScrubInvocation,
    class: ScrubIoClass,
    mount_point: &str,
) -> Vec<String> {
    let verb = match invocation {
        ScrubInvocation::Start | ScrubInvocation::ForcedStart => "start",
        ScrubInvocation::Resume => "resume",
    };
    let mut args = vec!["scrub".to_string(), verb.to_string(), "-B".to_string()];
    if invocation == ScrubInvocation::ForcedStart {
        args.push("-f".to_string());
    }
    args.extend(class.args().iter().map(|a| (*a).to_string()));
    args.push(mount_point.to_string());
    args
}

/// Run a scrub of `mount_point` in the foreground, resuming an interrupted pass
/// when one exists and otherwise starting fresh.
///
/// The resume-or-start decision is [`decide_scrub_start_mode`]. On the resume
/// path, if the kernel turns out to have no resumable state after all, this
/// falls back to `btrfs scrub start -B -f` — the pre-resume behaviour, which
/// clears a genuinely stale aborted record. The exit status is logged but never
/// used as the verdict — the status record is the source of truth, because a
/// scrub can exit zero and still have aborted.
fn run_btrfs_scrub(
    mount_point: &str,
    fsuuid: &str,
    progress: &dyn ProgressCallback,
    claimed: &dyn Fn() -> bool,
) -> ScrubRunOutcome {
    let mode = decide_scrub_start_mode(fsuuid, mount_point);
    run_btrfs_scrub_with(
        mode,
        &choose_scrub_io_class(Path::new(SYSFS_ROOT), fsuuid),
        mount_point,
        &|args| run_scrub_argv(args, progress, claimed),
        progress,
    )
}

/// [`run_btrfs_scrub`] with the start mode and I/O class decided and the
/// `btrfs` spawn supplied by the caller, so a test sees which argv each path
/// asks for. Logs the class once, before the first invocation; the
/// forced-start fallback follows the failed resume at once and keeps it.
fn run_btrfs_scrub_with(
    mode: ScrubStartMode,
    io: &ScrubIoChoice,
    mount_point: &str,
    run: &dyn Fn(&[String]) -> ScrubRunOutcome,
    progress: &dyn ProgressCallback,
) -> ScrubRunOutcome {
    progress.on_log(LogLevel::Info, &io.log_line(mount_point));
    let class = io.class;
    match mode {
        ScrubStartMode::Normal => run(&btrfs_scrub_args(
            ScrubInvocation::Start,
            class,
            mount_point,
        )),
        ScrubStartMode::Resume { reason } => {
            progress.on_log(
                LogLevel::Info,
                &format!("Resuming interrupted scrub: {reason}"),
            );
            let outcome = run(&btrfs_scrub_args(
                ScrubInvocation::Resume,
                class,
                mount_point,
            ));
            if resume_found_nothing(&outcome) {
                progress.on_log(
                    LogLevel::Warning,
                    &format!(
                        "'btrfs scrub resume' found no resumable state on {mount_point} — \
                         starting a fresh scrub with -f to clear the stale aborted record"
                    ),
                );
                run(&btrfs_scrub_args(
                    ScrubInvocation::ForcedStart,
                    class,
                    mount_point,
                ))
            } else {
                outcome
            }
        }
    }
}

/// Spawn `btrfs <args>`, drain its stderr, and poll to completion while logging
/// progress from the UUID-keyed status record. The `args` slice carries the
/// full argument vector including the mountpoint; the fsuuid for progress
/// reporting is recovered from the record itself, so this helper needs only the
/// argv and the progress sink.
fn run_scrub_argv(
    args: &[String],
    progress: &dyn ProgressCallback,
    claimed: &dyn Fn() -> bool,
) -> ScrubRunOutcome {
    // The mountpoint is the final argument; progress is keyed by the FS UUID of
    // whatever is mounted there.
    let mount_point = args.last().map(String::as_str).unwrap_or("");
    let fsuuid = mounted_fs_uuid(mount_point).unwrap_or_default();
    run_scrub_child(
        "btrfs",
        args,
        &fsuuid,
        SCRUB_POLL_INTERVAL,
        &pending_signal,
        claimed,
        progress,
    )
}

/// Ask a running scrub to leave (SIGTERM), give it [`CHILD_STOP_GRACE`], then
/// kill it; always reaped, so nothing keeps scrubbing a filesystem the caller
/// is about to unmount.
fn stop_child(child: &mut std::process::Child) {
    // SAFETY: signalling a child this process started and has not yet reaped.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + CHILD_STOP_GRACE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// [`run_scrub_argv`] with the program, the poll interval, the source of
/// "a termination signal arrived" and the source of "a backup claims the
/// maintenance lock" supplied by the caller, so a test drives a signal or a
/// claim at a stand-in child instead of a real scrub.
///
/// A claim is a second stop source beside `pending`, looked at before the
/// spawn and at every poll (bd DAS-Backup-Manager-su1b.2): the child is
/// stopped by [`stop_child`] (SIGTERM, which leaves a resumable record on
/// btrfs-progs 7.3, step A1) and reaped before this returns
/// [`ScrubRunOutcome::Yielded`]. A signal outranks a claim.
fn run_scrub_child(
    program: &str,
    args: &[String],
    fsuuid: &str,
    poll: Duration,
    pending: &dyn Fn() -> Option<i32>,
    claimed: &dyn Fn() -> bool,
    progress: &dyn ProgressCallback,
) -> ScrubRunOutcome {
    let mount_point = args.last().map(String::as_str).unwrap_or("");
    // Never start a scrub for a pass that has already been told to stop, or
    // that a backup has asked for the lock.
    if let Some(signal) = pending() {
        return ScrubRunOutcome::Interrupted(signal);
    }
    if claimed() {
        return ScrubRunOutcome::Yielded { launched: false };
    }
    let mut child = match Command::new(program)
        .args(args)
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
            Ok(None) => {
                if let Some(signal) = pending() {
                    stop_child(&mut child);
                    return ScrubRunOutcome::Interrupted(signal);
                }
                if claimed() {
                    // SIGTERM, then reaped: the record stays resumable
                    // (step A1). Never returns while the child lives.
                    stop_child(&mut child);
                    return ScrubRunOutcome::Yielded { launched: true };
                }
            }
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
        std::thread::sleep(poll);
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

    // The scrub ended unsuccessfully with a stop request pending: the signal
    // reached it (a supervisor signals the whole group) before the pass saw it.
    if !status.success()
        && let Some(signal) = pending()
    {
        return ScrubRunOutcome::Interrupted(signal);
    }

    ScrubRunOutcome::Launched(if status.success() {
        Ok(())
    } else {
        // The full argv is named so a "nothing to resume" from a resume attempt
        // is distinguishable in the message from a start failure — the string
        // `resume_found_nothing` keys on comes through in `stderr`.
        Err(format!(
            "btrfs {} exited {}{}",
            args.join(" "),
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
// The yield: a backup asks the pass to give the maintenance lock up
// (bd DAS-Backup-Manager-su1b.2, step A5)
// ---------------------------------------------------------------------------

/// What the pass knows of the claim lock ([`crate::maintenance::CLAIM_LOCK_PATH`]),
/// a backup's request that the pass let it go first.
///
/// A probe that cannot look (the file cannot be opened or locked) is read
/// as a claim once, while the pass holds the maintenance lock — the cautious
/// reading for the backup that may be waiting (`fail-silent.md`) — and then
/// as no claim until a probe works again, so a broken probe costs one pause,
/// never a pass that stops every few seconds or waits outside for ever.
/// Safety never rests on the claim: only the maintenance lock decides who
/// mounts.
pub(crate) struct ClaimWatch {
    path: PathBuf,
    broken: std::cell::Cell<bool>,
    /// The pause request file the pass also obeys (bd DAS-Backup-Manager-su1b.15).
    pause_request: Option<PathBuf>,
}

impl ClaimWatch {
    pub(crate) fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            broken: std::cell::Cell::new(false),
            pause_request: None,
        }
    }

    /// Watch the pause request at `path` as well.
    pub(crate) fn with_pause_request(mut self, path: &Path) -> Self {
        self.pause_request = Some(path.to_path_buf());
        self
    }

    /// The pause request, when one is in place.
    pub(crate) fn pause_request(&self) -> Option<PauseRequest> {
        self.pause_request.as_deref().and_then(read_pause_request)
    }

    /// At checkpoints 3 and 4: a backup wants the lock, or a pause was asked
    /// for — either way the pass yields. The pause file is not part of the
    /// waits for the lock ([`take_maintenance_behind_claims`]): a pass never
    /// waits on it, it ends on it.
    pub(crate) fn wants_lock_or_pause(&self, progress: &dyn ProgressCallback) -> bool {
        self.wants_lock(progress) || self.pause_request().is_some()
    }

    /// One probe: `Some(held)` when it could look (and the probe works
    /// again), `None` when it could not — said once, when it starts failing.
    fn probe(&self, progress: &dyn ProgressCallback) -> Option<bool> {
        match crate::maintenance::claim_is_held(&self.path) {
            Ok(held) => {
                self.broken.set(false);
                Some(held)
            }
            Err(e) => {
                if !self.broken.get() {
                    progress.on_log(
                        LogLevel::Warning,
                        &format!(
                            "cannot look at the claim lock {} ({e}) — a pass holding the \
                             maintenance lock gives it up once, as if a backup had asked; until \
                             the claim can be looked at again, it neither waits for one nor \
                             gives the lock up for one",
                            self.path.display()
                        ),
                    );
                }
                None
            }
        }
    }

    /// While the pass holds the maintenance lock: whether a backup wants it.
    pub(crate) fn wants_lock(&self, progress: &dyn ProgressCallback) -> bool {
        let was_broken = self.broken.get();
        let seen = self.probe(progress);
        let now_broken = seen.is_none();
        self.broken.set(now_broken);
        // A probe that has just started failing is read as a claim, once.
        seen.unwrap_or(!was_broken)
    }

    /// While the pass waits outside: whether a backup still waits to go
    /// first. A probe that cannot look does not keep the pass waiting.
    pub(crate) fn still_waiting(&self, progress: &dyn ProgressCallback) -> bool {
        let seen = self.probe(progress);
        self.broken.set(seen.is_none());
        seen.unwrap_or(false)
    }

    /// The claimant, as it recorded itself (`backup-run.sh pid 1234`).
    pub(crate) fn holder(&self) -> String {
        crate::maintenance::holder_of(&self.path)
    }
}

// ---------------------------------------------------------------------------
// Pause and resume by hand (bd DAS-Backup-Manager-su1b.15)
// ---------------------------------------------------------------------------

/// A pause asked for by `btrdasd scrub pause`, as its file records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseRequest {
    /// Who asked (`btrdasd scrub pause pid 4242, by user`). Display only.
    pub by: String,
    /// When it was asked; `None` when the file does not say.
    pub epoch: Option<i64>,
}

/// The pause request at `path`, when the file exists. A file that exists but
/// cannot be read or parsed is still a request (the cautious reading: the
/// pass pauses), naming what was wrong in `by`.
pub fn read_pause_request(path: &Path) -> Option<PauseRequest> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        _ => {}
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            return Some(PauseRequest {
                by: format!("an unreadable pause request {} ({e})", path.display()),
                epoch: None,
            });
        }
    };
    let mut by = None;
    let mut epoch = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("requested_by ") {
            by = Some(v.chars().filter(|c| !c.is_control()).take(200).collect());
        } else if let Some(v) = line.strip_prefix("requested_epoch ") {
            epoch = v.trim().parse().ok();
        }
    }
    Some(PauseRequest {
        by: by.unwrap_or_else(|| format!("a pause request {} that names nobody", path.display())),
        epoch,
    })
}

/// Write the pause request at `path`, mode 0600, never through a link.
pub fn write_pause_request(path: &Path, by: &str, epoch: i64) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    writeln!(file, "requested_by {by}")?;
    writeln!(file, "requested_epoch {epoch}")?;
    file.sync_all()
}

/// Remove the pause request at `path`: `Ok(true)` when one was there.
pub fn remove_pause_request(path: &Path) -> std::io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The paths and timings `btrdasd scrub pause` and `resume` work with.
#[derive(Debug, Clone)]
pub struct PausePaths {
    /// The scrub singleton (`/run/das-scrub.lock`).
    pub singleton: PathBuf,
    /// The claim lock (`/run/das-maintenance-claim.lock`).
    pub claim: PathBuf,
    /// The pause request (`/run/das-scrub-pause`).
    pub request: PathBuf,
    /// The scrub state file.
    pub state: PathBuf,
}

impl PausePaths {
    /// The installed paths.
    pub fn production() -> Self {
        Self {
            singleton: overridable("DAS_SCRUB_LOCK", SCRUB_LOCK_PATH),
            claim: claim_lock_path(),
            request: pause_request_path(),
            state: state_path(),
        }
    }
}

/// Who holds the scrub singleton now: `None` when nobody does (looked at by
/// taking it and letting go at once, as `scrub cancel` does).
fn singleton_holder(path: &Path) -> Result<Option<String>, ScrubError> {
    match FileLock::try_acquire(path)? {
        Some(_free) => Ok(None),
        None => Ok(Some(crate::maintenance::holder_of(path))),
    }
}

/// How `btrdasd scrub pause` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseOutcome {
    /// No pass was running: nothing written, nothing to pause.
    NotRunning,
    /// `btrdasd setup` holds the singleton, not a pass: nothing written.
    SetupHolds(String),
    /// The pass ended, these filesystems recorded `paused`.
    Paused(Vec<String>),
    /// The pass ended without pausing anything (it finished first): the
    /// request was taken away again, so the next pass runs.
    EndedUnpaused,
    /// The pass did not end within the bound; the request stays, so it
    /// pauses at its next look (every few seconds while it scrubs).
    NoAnswer(String),
}

/// `btrdasd scrub pause`: write the request, then claim the maintenance lock
/// as a backup does, so the running pass stops its scrub resumably,
/// unmounts and yields; seeing the request it ends `paused`. Waits up to
/// `bound`, looking every `poll`, for the singleton to be let go; the claim
/// is let go at the end either way. Kills nothing.
pub fn pause_pass_at(
    paths: &PausePaths,
    by: &str,
    bound: Duration,
    poll: Duration,
) -> Result<PauseOutcome, ScrubError> {
    let Some(holder) = singleton_holder(&paths.singleton)? else {
        return Ok(PauseOutcome::NotRunning);
    };
    if holder_is_setup(&holder) {
        return Ok(PauseOutcome::SetupHolds(holder));
    }
    write_pause_request(&paths.request, by, now_epoch()).map_err(|e| ScrubError::Lock {
        path: paths.request.display().to_string(),
        detail: format!("cannot write the pause request: {e}"),
    })?;
    let claim = crate::maintenance::Claim::take_at(&paths.claim, "btrdasd scrub pause")?;
    let deadline = Instant::now() + bound;
    let ended = loop {
        if singleton_holder(&paths.singleton)?.is_none() {
            break true;
        }
        let now = Instant::now();
        if now >= deadline {
            break false;
        }
        std::thread::sleep(poll.min(deadline - now));
    };
    drop(claim);
    if !ended {
        return Ok(PauseOutcome::NoAnswer(crate::maintenance::holder_of(
            &paths.singleton,
        )));
    }
    let paused: Vec<String> = load_state_from(&paths.state)
        .map(|s| {
            s.filesystems
                .values()
                .filter(|f| f.last_attempt.outcome == "paused")
                .map(|f| f.target_label.clone())
                .collect()
        })
        .unwrap_or_default();
    if paused.is_empty() {
        remove_pause_request(&paths.request).map_err(|e| ScrubError::Lock {
            path: paths.request.display().to_string(),
            detail: format!("cannot remove the pause request: {e}"),
        })?;
        return Ok(PauseOutcome::EndedUnpaused);
    }
    Ok(PauseOutcome::Paused(paused))
}

/// How `btrdasd scrub resume` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// A pass (or setup) holds the singleton: nothing removed, nothing started.
    AlreadyRunning(String),
    /// The request (if any: `removed`) is gone and the pass was started.
    Started { removed: bool },
}

/// `btrdasd scrub resume`: refuse while anything holds the singleton; else
/// remove the pause request and `start` the pass (production: `systemctl
/// start --no-block das-scrub.service`), which resumes a filesystem whose
/// btrfs record is resumable ([`decide_scrub_start_mode`]).
pub fn resume_pass_at(
    paths: &PausePaths,
    start: &dyn Fn() -> Result<(), String>,
) -> Result<ResumeOutcome, String> {
    if let Some(holder) = singleton_holder(&paths.singleton).map_err(|e| e.to_string())? {
        return Ok(ResumeOutcome::AlreadyRunning(holder));
    }
    let removed = remove_pause_request(&paths.request).map_err(|e| {
        format!(
            "cannot remove the pause request {}: {e}",
            paths.request.display()
        )
    })?;
    start()?;
    Ok(ResumeOutcome::Started { removed })
}

/// A pass that ran to its end with a pause request it never met (one that
/// came after its last checkpoint) takes it away, so the next pass is not
/// left paused with no `paused` record to show for it.
fn lift_unmet_pause_request(request: &Path, progress: &dyn ProgressCallback) {
    match remove_pause_request(request) {
        Ok(true) => progress.on_log(
            LogLevel::Info,
            &format!(
                "The pass ended before it could pause: the pause request {} is taken away",
                request.display()
            ),
        ),
        Ok(false) => {}
        Err(e) => progress.on_log(
            LogLevel::Warning,
            &format!(
                "The pass ended before it could pause, and the pause request {} could not be \
                 taken away ({e}): the next pass will not run until `btrdasd scrub resume`",
                request.display()
            ),
        ),
    }
}

/// A pass that starts while a pause request is in place does not run
/// (bd DAS-Backup-Manager-su1b.15): it takes no lock, removes nothing and
/// writes nothing; a paused scrub stays paused until `btrdasd scrub resume`.
fn paused_at_start(
    request: &Path,
    started_epoch: i64,
    progress: &dyn ProgressCallback,
) -> Option<ScrubPass> {
    let req = read_pause_request(request)?;
    let line = format!(
        "The scrub is paused ({}, by {}): this pass does not run — `btrdasd scrub resume` \
         lifts the pause and starts it",
        request.display(),
        req.by
    );
    progress.on_log(LogLevel::Warning, &line);
    progress.on_complete(true, "Scrub paused — not run");
    Some(ScrubPass {
        status: PassStatus::Paused,
        started_epoch,
        finished_epoch: now_epoch(),
        results: Vec::new(),
        errors: Vec::new(),
        already_scrubbed: Vec::new(),
    })
}

/// What the stopped segment's state allows, once its child is reaped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum YieldVerdict {
    /// Unmount, give the lock up, resume this filesystem afterwards. `note`
    /// says why the resume may start from zero (a canceled record, or none
    /// written by this segment yet); `None` when the record is resumable.
    Pause { note: Option<String> },
    /// The filesystem finished as it was stopped: a result like any other.
    Finished,
    /// The kernel was not seen to stop the scrub: the mount is left as it
    /// is, the lock given up, and this filesystem not scrubbed again in this
    /// pass (design A-F8).
    KernelLive(String),
}

/// Decide [`YieldVerdict`] from whether a child ran, what the kernel says
/// now, and the record as read by UUID. A record whose last start or resume
/// predates this segment's start is not this segment's (the 2026-07-27
/// stale-record trap): it is never read as a finished result.
fn yield_verdict(
    launched: bool,
    live: LiveScrubState,
    record: Option<&ScrubStatusRecord>,
    segment_started: i64,
) -> YieldVerdict {
    if !launched {
        return YieldVerdict::Pause { note: None };
    }
    if live != LiveScrubState::NotRunning {
        // `Unknown` is as good as `Running` here: an unmount under a live
        // scrub is refused anyway, and nothing may act on a guess.
        return YieldVerdict::KernelLive(format!(
            "the kernel was not seen to stop this filesystem's scrub after its btrfs child \
             was stopped and reaped (scrub status: {live:?}) — left mounted for the next job's \
             leftover check, and not resumed in this pass"
        ));
    }
    match record {
        Some(r)
            if r.last_activity_start() != 0
                && r.last_activity_start() >= segment_started - RECORD_FRESHNESS_SLACK_SECS =>
        {
            match r.outcome() {
                ScrubOutcome::Finished => YieldVerdict::Finished,
                ScrubOutcome::Aborted => YieldVerdict::Pause { note: None },
                ScrubOutcome::Canceled => YieldVerdict::Pause {
                    note: Some(
                        "the stopped scrub left a canceled record (canceled:1): the resume \
                         will start this filesystem from zero — no region is skipped, only \
                         work repeated"
                            .to_string(),
                    ),
                },
            }
        }
        _ => YieldVerdict::Pause {
            note: Some(
                "the stopped scrub had not yet written a record of its own: the resume \
                 decides from the earlier record, and may start this filesystem afresh"
                    .to_string(),
            ),
        },
    }
}

/// The pass's hold on the maintenance lock, which it gives up for a waiting
/// backup and takes back: the only owner of the pass's [`MaintenanceHeld`].
/// Every mount still needs a `&MaintenanceHeld`, which only a held slot
/// gives.
#[derive(Debug)]
pub struct MaintenanceSlot {
    path: PathBuf,
    poll: Duration,
    held: Option<MaintenanceHeld>,
}

impl MaintenanceSlot {
    /// The hold, while the pass has it.
    pub fn held(&self) -> Option<&MaintenanceHeld> {
        self.held.as_ref()
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// How a wait for the maintenance lock, behind any claim, ended.
#[derive(Debug)]
enum Retake {
    Held(MaintenanceHeld),
    Interrupted(i32),
    /// A pause request is in place (bd DAS-Backup-Manager-su1b.15): the pass
    /// ends `paused` instead of waiting to resume.
    Paused(PauseRequest),
}

/// Take the maintenance lock at `path` for the pass, behind any backup that
/// claims it (design checkpoints 1 and 2): while `still_waiting()`, wait;
/// once it is not, try the lock; taken, but `wants_lock()` now says a backup
/// claimed it meanwhile, let it go unused and wait again. Every `poll`,
/// `stop()` is looked at, so a signal ends the wait at once — a blocking
/// `flock` would retry through it. Logs the wait after
/// [`LOCK_WAIT_ANNOUNCE_SECS`], again every [`LOCK_WAIT_REANNOUNCE_SECS`].
fn take_maintenance_behind_claims(
    path: &Path,
    poll: Duration,
    still_waiting: &dyn Fn() -> bool,
    wants_lock: &dyn Fn() -> bool,
    stop: &dyn Fn() -> Option<i32>,
    paused: &dyn Fn() -> Option<PauseRequest>,
    progress: &dyn ProgressCallback,
) -> Result<Retake, ScrubError> {
    let started = Instant::now();
    let mut next_notice = LOCK_WAIT_ANNOUNCE_SECS;
    let mut announced = false;
    loop {
        if let Some(signal) = stop() {
            return Ok(Retake::Interrupted(signal));
        }
        if let Some(request) = paused() {
            return Ok(Retake::Paused(request));
        }
        let claimed = still_waiting();
        if !claimed && let Some(held) = MaintenanceHeld::try_acquire_at(path, SCRUB_JOB)? {
            if !wants_lock() {
                if announced {
                    progress.on_log(
                        LogLevel::Info,
                        &format!(
                            "DAS maintenance lock acquired after waiting {}s",
                            started.elapsed().as_secs()
                        ),
                    );
                }
                return Ok(Retake::Held(held));
            }
            // A backup claimed it as it was taken: let it go unused.
            drop(held);
        }
        let waited = started.elapsed().as_secs();
        if waited >= next_notice {
            let line = if claimed {
                format!(
                    "Waiting: a backup claims the DAS maintenance lock {} — this pass lets it go \
                     first",
                    path.display()
                )
            } else {
                crate::maintenance::blocked_line(path)
            };
            if announced {
                progress.on_log(
                    LogLevel::Info,
                    &format!("{line} — still waiting after {}m", waited / 60),
                );
            } else {
                progress.on_log(LogLevel::Info, &line);
            }
            announced = true;
            next_notice = if next_notice < LOCK_WAIT_REANNOUNCE_SECS {
                LOCK_WAIT_REANNOUNCE_SECS
            } else {
                next_notice + LOCK_WAIT_REANNOUNCE_SECS
            };
        }
        std::thread::sleep(poll);
    }
}

/// What the pass writes when it gives the lock up: a `yielded` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YieldMark {
    pub(crate) label: String,
    pub(crate) fsuuid: String,
    pub(crate) mountpoint: String,
    pub(crate) started_epoch: i64,
    pub(crate) at_epoch: i64,
    pub(crate) holder: String,
    pub(crate) yields: u32,
    pub(crate) paused_secs: u64,
}

/// Scrub one target under the hold: [`scrub_one_target`] in production.
type ScrubOne<'a> = dyn Fn(&Config, &str, &dyn ProgressCallback, &MaintenanceHeld, &dyn Fn() -> bool) -> ScrubFsResult
    + 'a;

/// What the pass loop calls out to, so a test drives each.
pub(crate) struct PassHooks<'a> {
    /// The signal the pass caught, if any.
    pub(crate) stop: &'a dyn Fn() -> Option<i32>,
    /// Read `config.toml` again, after a pause.
    pub(crate) reload: &'a dyn Fn() -> Result<Config, String>,
    pub(crate) scrub_one: &'a ScrubOne<'a>,
    /// File a filesystem's result the moment it is done.
    pub(crate) file_result: &'a dyn Fn(&ScrubFsResult),
    /// File a `yielded` record as the pass gives the lock up.
    pub(crate) file_yield: &'a dyn Fn(&YieldMark),
}

/// One filesystem's scrub across the segments its pauses cut it into.
#[derive(Debug, Default)]
struct Segments {
    label: String,
    fsuuid: String,
    mountpoint: String,
    started_epoch: i64,
    launched: bool,
    yields: Vec<ScrubYield>,
    errors: Vec<String>,
}

impl Segments {
    fn for_label(label: &str, config: &Config) -> Self {
        Self {
            label: label.to_string(),
            fsuuid: resolve_target_fsuuid(config, label).unwrap_or_default(),
            mountpoint: config
                .targets
                .iter()
                .find(|t| t.label == label)
                .map(|t| t.mount.clone())
                .unwrap_or_default(),
            ..Self::default()
        }
    }

    /// Fold a segment that stopped for a claim into this filesystem's scrub.
    fn add(&mut self, segment: &ScrubFsResult) {
        if !segment.fsuuid.is_empty() {
            self.fsuuid = segment.fsuuid.clone();
        }
        if !segment.mountpoint.is_empty() {
            self.mountpoint = segment.mountpoint.clone();
        }
        if self.started_epoch == 0 {
            self.started_epoch = segment.started_epoch;
        }
        self.launched |= segment.scrub_launched;
        self.errors.extend(segment.errors.iter().cloned());
    }

    fn paused_secs(&self) -> u64 {
        self.yields.iter().filter_map(|y| y.paused_secs).sum()
    }

    /// The `yielded` record of a pause about to begin (already in `yields`).
    fn mark(&self) -> YieldMark {
        let current = self.yields.last();
        YieldMark {
            label: self.label.clone(),
            fsuuid: self.fsuuid.clone(),
            mountpoint: self.mountpoint.clone(),
            started_epoch: self.started_epoch,
            at_epoch: current.map_or_else(now_epoch, |y| y.at_epoch),
            holder: current.map(|y| y.holder.clone()).unwrap_or_default(),
            yields: self.yields.len() as u32,
            paused_secs: self.paused_secs(),
        }
    }

    /// This filesystem's result: `last` (the last segment's, or a fresh one
    /// for a filesystem the pass could not go back to) with what the earlier
    /// segments did.
    fn finish(self, mut last: ScrubFsResult) -> ScrubFsResult {
        last.pending_yield = None;
        if last.fsuuid.is_empty() {
            last.fsuuid = self.fsuuid;
        }
        if last.mountpoint.is_empty() {
            last.mountpoint = self.mountpoint;
        }
        if last.started_epoch == 0 {
            last.started_epoch = self.started_epoch;
        }
        last.scrub_launched |= self.launched;
        let mut errors = self.errors;
        errors.append(&mut last.errors);
        last.errors = errors;
        let mut yields = self.yields;
        yields.append(&mut last.yields);
        last.yields = yields;
        last
    }
}

/// How a pause ended.
enum PauseEnd {
    /// The lock is the pass's again after this many seconds.
    Resumed(u64),
    Interrupted(i32),
    LockFailed(String),
    /// A pause request ends the pass (bd DAS-Backup-Manager-su1b.15).
    Paused(PauseRequest),
}

/// Give the lock up for the claimant (its record cleared, then unlocked),
/// wait behind it and any other claim, and take the lock back.
fn pause_for_claim(
    slot: &mut MaintenanceSlot,
    claim: &ClaimWatch,
    stop: &dyn Fn() -> Option<i32>,
    progress: &dyn ProgressCallback,
) -> PauseEnd {
    slot.held = None;
    let started = Instant::now();
    match take_maintenance_behind_claims(
        &slot.path,
        slot.poll,
        &|| claim.still_waiting(progress),
        &|| claim.wants_lock(progress),
        stop,
        &|| claim.pause_request(),
        progress,
    ) {
        Ok(Retake::Held(held)) => {
            slot.held = Some(held);
            PauseEnd::Resumed(started.elapsed().as_secs())
        }
        Ok(Retake::Interrupted(signal)) => PauseEnd::Interrupted(signal),
        Ok(Retake::Paused(request)) => PauseEnd::Paused(request),
        Err(e) => PauseEnd::LockFailed(e.to_string()),
    }
}

/// Scrub every `[scrub].targets` entry in turn, giving the maintenance lock
/// up whenever a backup claims it, and resuming the same filesystem once it
/// is back (design §3.2):
///
/// - checkpoint 3, before each filesystem's mount: a claim pauses the pass
///   with nothing mounted, and a `yielded` record names the filesystem next
///   in line, so a pass that dies there is not silent;
/// - checkpoint 4, inside the scrub ([`run_scrub_child`]): the child is
///   stopped and reaped and the filesystem unmounted by `scrub_one`
///   ([`scrub_one_target`]); only then is the lock given up here;
/// - checkpoints 1 and 2 are [`take_maintenance_behind_claims`], at every
///   return to the lock (and at the pass's start, [`acquire_locks_at`]).
///
/// After each pause `config.toml` is read again before anything is mounted:
/// the backup that ran may have changed it (its subvolume sync does). A
/// configuration that cannot be read ends the pass, the paused filesystem
/// failing with the reason; a filesystem the new configuration no longer
/// scrubs is not resumed, and says so.
///
/// `already_scrubbed` ([`labels_already_scrubbed`]) are left out of the queue:
/// the paused pass this one resumes scrubbed them to the end (bd
/// DAS-Backup-Manager-su1b.24).
fn scrub_targets_yielding(
    config: &Config,
    progress: &dyn ProgressCallback,
    slot: &mut MaintenanceSlot,
    claim: &ClaimWatch,
    already_scrubbed: &[String],
    hooks: &PassHooks,
) -> Vec<ScrubFsResult> {
    let mut config = config.clone();
    let mut queue: std::collections::VecDeque<String> = config
        .scrub_target_labels()
        .into_iter()
        .filter(|label| !already_scrubbed.contains(label))
        .collect();
    progress.on_stage("Scrubbing DAS filesystems", queue.len() as u64);

    let mut results: Vec<ScrubFsResult> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    // The filesystem whose scrub a pause cut, while it waits to resume.
    let mut cut: Option<Segments> = None;
    // A filesystem filed already whose pause (A-F8) is yet to end.
    let mut filed_pause: Option<usize> = None;
    let mut step = 0u64;

    let fail = |results: &mut Vec<ScrubFsResult>, result: ScrubFsResult| {
        (hooks.file_result)(&result);
        results.push(result);
    };

    'targets: while let Some(label) = queue.front().cloned() {
        let resuming = cut.as_ref().is_some_and(|c| c.label == label);
        if !resuming {
            if let Some(signal) = (hooks.stop)() {
                // Caught between targets: this one is listed as not begun, so the
                // pass reads as interrupted, and nothing after it starts.
                let mut result = ScrubFsResult::new(&label);
                result.interrupted_by = Some(signal);
                result.errors.push(format!(
                    "not started: the pass was interrupted by {}",
                    signal_name(signal)
                ));
                progress.on_log(
                    LogLevel::Error,
                    &format!(
                        "{label}: scrub not started — interrupted by {}",
                        signal_name(signal)
                    ),
                );
                results.push(result);
                break;
            }
            step += 1;
            progress.on_progress(
                step,
                (done.len() + queue.len()) as u64,
                &format!("Scrubbing {label}"),
            );
        }

        // Checkpoint 3: a claim before the mount pauses with nothing mounted.
        let pause_now = claim.wants_lock_or_pause(progress);
        let result = if pause_now {
            None
        } else {
            let Some(held) = slot.held.as_ref() else {
                // Unreachable while the loop takes the lock back before going
                // on; a pass without it mounts nothing.
                let mut result = ScrubFsResult::new(&label);
                result
                    .errors
                    .push("the pass does not hold the maintenance lock — not scrubbed".into());
                fail(&mut results, result);
                break;
            };
            Some((hooks.scrub_one)(&config, &label, progress, held, &|| {
                claim.wants_lock_or_pause(progress)
            }))
        };

        let pending = result.as_ref().and_then(|r| r.pending_yield);
        if let (Some(r), None) = (result.as_ref(), pending) {
            // The filesystem is done.
            let r = match cut.take() {
                Some(segments) if segments.label == label => segments.finish(r.clone()),
                other => {
                    cut = other;
                    r.clone()
                }
            };
            let interrupted = r.interrupted_by.is_some();
            if r.ok() {
                progress.on_log(
                    LogLevel::Info,
                    &format!(
                        "{label}: scrub finished cleanly ({} in {}s)",
                        format_bytes(r.bytes_scrubbed),
                        r.duration_secs
                    ),
                );
            } else {
                progress.on_log(
                    LogLevel::Error,
                    &format!("{label}: scrub FAILED — {}", describe_failure(&r)),
                );
            }
            (hooks.file_result)(&r);
            results.push(r);
            done.push(label.clone());
            queue.pop_front();
            if interrupted {
                break;
            }
            continue;
        }

        // A pause: before the mount (`result` is None) or a stopped segment.
        let holder = claim.holder();
        let at_epoch = now_epoch();
        let this_yield = ScrubYield {
            at_epoch,
            holder: holder.clone(),
            paused_secs: None,
        };
        match (result, pending) {
            (Some(segment), Some(PendingYield { resume: false })) => {
                // A-F8: filed now, with its error; not resumed in this pass.
                let segments = match cut.take() {
                    Some(s) if s.label == label => s,
                    other => {
                        cut = other;
                        Segments::for_label(&label, &config)
                    }
                };
                let mut r = segments.finish(segment);
                r.yields.push(this_yield);
                progress.on_log(
                    LogLevel::Error,
                    &format!("{label}: scrub FAILED — {}", describe_failure(&r)),
                );
                (hooks.file_result)(&r);
                results.push(r);
                filed_pause = Some(results.len() - 1);
                done.push(label.clone());
                queue.pop_front();
            }
            (segment, _) => {
                let mut segments = match cut.take() {
                    Some(s) if s.label == label => s,
                    _ => Segments::for_label(&label, &config),
                };
                if let Some(segment) = segment.as_ref() {
                    segments.add(segment);
                }
                segments.yields.push(this_yield);
                (hooks.file_yield)(&segments.mark());
                cut = Some(segments);
            }
        }
        progress.on_log(
            LogLevel::Info,
            &format!(
                "{label}: giving the DAS maintenance lock up to {holder}, which claims it; \
                 this pass resumes once it is done"
            ),
        );

        // The pause itself.
        let end = pause_for_claim(slot, claim, hooks.stop, progress);
        let paused = match end {
            PauseEnd::Resumed(secs) => secs,
            PauseEnd::Paused(request) => {
                // Paused by hand (bd DAS-Backup-Manager-su1b.15): the
                // filesystem cut (or, after an A-F8 filing, the next one) is
                // filed `paused` - resumable, not a failure - and the pass
                // ends without waiting, holding no maintenance lock.
                let Some(label) = cut
                    .as_ref()
                    .map(|s| s.label.clone())
                    .or_else(|| queue.front().cloned())
                else {
                    break;
                };
                let mut r = match cut.take() {
                    Some(s) => s.finish(ScrubFsResult::new(&label)),
                    None => Segments::for_label(&label, &config).finish(ScrubFsResult::new(&label)),
                };
                r.paused = Some(PausedBy {
                    by: request.by.clone(),
                    at_epoch: now_epoch(),
                });
                progress.on_log(
                    LogLevel::Info,
                    &format!(
                        "{label}: scrub PAUSED at the request of {} - the pass ends here; \
                         `btrdasd scrub resume` resumes it",
                        request.by
                    ),
                );
                (hooks.file_result)(&r);
                results.push(r);
                break;
            }
            PauseEnd::Interrupted(signal) => {
                let mut r = match cut.take() {
                    Some(s) => {
                        let label = s.label.clone();
                        s.finish(ScrubFsResult::new(&label))
                    }
                    None => match queue.front() {
                        Some(next) => ScrubFsResult::new(next),
                        None => {
                            // Nothing left to begin: the last filesystem,
                            // filed already (A-F8), carries the signal.
                            if let Some(last) = results.last_mut() {
                                last.interrupted_by = Some(signal);
                                (hooks.file_result)(last);
                            }
                            break 'targets;
                        }
                    },
                };
                r.interrupted_by = Some(signal);
                r.errors.push(format!(
                    "interrupted by {} while paused for a backup",
                    signal_name(signal)
                ));
                progress.on_log(
                    LogLevel::Error,
                    &format!(
                        "{}: interrupted by {} while paused for a backup",
                        r.target_label,
                        signal_name(signal)
                    ),
                );
                fail(&mut results, r);
                break;
            }
            PauseEnd::LockFailed(why) => {
                let label = cut
                    .as_ref()
                    .map(|s| s.label.clone())
                    .or_else(|| queue.front().cloned());
                let Some(label) = label else { break };
                let mut r = match cut.take() {
                    Some(s) => s.finish(ScrubFsResult::new(&label)),
                    None => ScrubFsResult::new(&label),
                };
                r.errors.push(format!(
                    "the DAS maintenance lock could not be taken back after the pause: {why} \
                     — not resumed; this pass stops here"
                ));
                progress.on_log(
                    LogLevel::Error,
                    &format!("{label}: {}", r.errors.last().unwrap()),
                );
                fail(&mut results, r);
                break;
            }
        };
        if let Some(segments) = cut.as_mut()
            && let Some(last) = segments.yields.last_mut()
        {
            last.paused_secs = Some(paused);
        }
        if let Some(i) = filed_pause.take()
            && let Some(last) = results[i].yields.last_mut()
        {
            last.paused_secs = Some(paused);
        }
        progress.on_log(
            LogLevel::Info,
            &format!("{label}: the DAS maintenance lock is back after a pause of {paused}s"),
        );

        // Read the configuration again before anything is mounted.
        match (hooks.reload)() {
            Ok(fresh) => config = fresh,
            Err(why) => {
                let Some(label) = cut
                    .as_ref()
                    .map(|s| s.label.clone())
                    .or_else(|| queue.front().cloned())
                else {
                    break;
                };
                let mut r = match cut.take() {
                    Some(s) => s.finish(ScrubFsResult::new(&label)),
                    None => ScrubFsResult::new(&label),
                };
                r.errors.push(format!(
                    "config.toml could not be read again after the pause for a backup ({why}) \
                     — {label} was not resumed; this pass stops here"
                ));
                progress.on_log(
                    LogLevel::Error,
                    &format!("{label}: {}", r.errors.last().unwrap()),
                );
                fail(&mut results, r);
                break;
            }
        }
        queue = config
            .scrub_target_labels()
            .into_iter()
            .filter(|l| !done.contains(l))
            .collect();
        if let Some(segments) = cut.take_if(|s| !queue.contains(&s.label)) {
            let label = segments.label.clone();
            let mut r = segments.finish(ScrubFsResult::new(&label));
            r.errors.push(format!(
                "{label} is no longer a [scrub] target in config.toml as read again after the \
                 pause for a backup — not resumed"
            ));
            progress.on_log(
                LogLevel::Error,
                &format!("{label}: {}", r.errors.last().unwrap()),
            );
            fail(&mut results, r);
            done.push(label);
        }
        // The cut filesystem goes first, wherever the new list puts it.
        if let Some(segments) = cut.as_ref()
            && let Some(at) = queue.iter().position(|l| *l == segments.label)
            && let Some(l) = queue.remove(at)
        {
            queue.push_front(l);
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Termination signals and the "running" record
// ---------------------------------------------------------------------------

/// The termination signal the pass caught (0 = none).
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// The signals a stop request arrives as.
const CAUGHT_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// How long a signalled `btrfs scrub` gets to leave before it is killed.
const CHILD_STOP_GRACE: Duration = Duration::from_secs(30);

extern "C" fn remember_signal(signal: libc::c_int) {
    // An atomic store is async-signal-safe; nothing else happens here.
    PENDING_SIGNAL.store(signal, Ordering::SeqCst);
}

/// The termination signal the pass caught, if any.
pub fn pending_signal() -> Option<i32> {
    match PENDING_SIGNAL.load(Ordering::SeqCst) {
        0 => None,
        signal => Some(signal),
    }
}

/// Keeps the pass's handlers installed; dropping it puts the old ones back
/// and forgets what was caught.
pub struct TerminationGuard {
    old: Vec<(libc::c_int, libc::sigaction)>,
}

/// Catch SIGTERM, SIGINT and SIGHUP for the length of the returned guard, so
/// the pass can stop its scrub, record why, unmount, and only then die of the
/// signal ([`die_of_signal`]) instead of vanishing without a trace.
pub fn catch_termination() -> TerminationGuard {
    PENDING_SIGNAL.store(0, Ordering::SeqCst);
    let mut old = Vec::new();
    for signal in CAUGHT_SIGNALS {
        // SAFETY: `sigaction` is plain data; zeroed, then every field the
        // call reads is set. The handler only stores into an atomic.
        let mut new: libc::sigaction = unsafe { std::mem::zeroed() };
        new.sa_sigaction = remember_signal as extern "C" fn(libc::c_int) as usize;
        new.sa_flags = 0;
        // SAFETY: `new.sa_mask` is a valid set to initialise.
        unsafe { libc::sigemptyset(&mut new.sa_mask) };
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are valid; `signal` is a valid signal number.
        if unsafe { libc::sigaction(signal, &new, &mut previous) } == 0 {
            old.push((signal, previous));
        }
    }
    TerminationGuard { old }
}

impl Drop for TerminationGuard {
    fn drop(&mut self) {
        for (signal, previous) in &self.old {
            // SAFETY: restoring the action `sigaction` itself returned.
            unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) };
        }
        PENDING_SIGNAL.store(0, Ordering::SeqCst);
    }
}

/// End this process the way the signal would have, now that the pass has
/// recorded it: default action, then the shell's `128 + N` as a fallback.
/// The exit status a supervisor sees is the same as before the pass learned to
/// record the signal.
pub fn die_of_signal(signal: i32) -> ! {
    // SAFETY: restoring the default action and raising a signal this process
    // may receive at any time.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
    std::process::exit(128 + signal)
}

/// How a signal number reads in the state file and the log.
pub fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGTERM => "SIGTERM".to_string(),
        libc::SIGINT => "SIGINT".to_string(),
        libc::SIGHUP => "SIGHUP".to_string(),
        other => format!("signal {other}"),
    }
}

/// This boot's id, which tells a `running` record of an earlier boot from one
/// of this boot's.
pub fn current_boot_id() -> Option<String> {
    let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// What a `running` record is written from.
struct RunningMarker<'a> {
    label: &'a str,
    fsuuid: &'a str,
    mountpoint: &'a str,
    started_epoch: i64,
}

/// Write a `running` attempt for the filesystem about to be scrubbed, before
/// the scrub starts: whatever then ends the pass without a word (SIGKILL, a
/// power cut) still leaves `status` something that is not the earlier pass.
/// The earlier success stays, so ageing is unchanged.
fn record_running(
    path: &Path,
    marker: &RunningMarker,
    progress: &dyn ProgressCallback,
) -> Result<(), ScrubError> {
    let mut state = load_state_for_write(path, progress)?;
    let last_success_epoch = state
        .filesystems
        .get(marker.fsuuid)
        .and_then(|f| f.last_success_epoch);
    state.filesystems.insert(
        marker.fsuuid.to_string(),
        FsState {
            target_label: marker.label.to_string(),
            mountpoint: marker.mountpoint.to_string(),
            last_success_epoch,
            last_attempt: AttemptState {
                outcome: "running".to_string(),
                started_epoch: marker.started_epoch,
                pid: Some(std::process::id()),
                boot_id: current_boot_id(),
                ..Default::default()
            },
        },
    );
    save_state_to(&state, path)
}

/// [`record_running`], with a failure logged: the pass goes on without the
/// marker, and the log says so.
fn record_running_logged(path: &Path, marker: &RunningMarker, progress: &dyn ProgressCallback) {
    if let Err(e) = record_running(path, marker, progress) {
        progress.on_log(
            LogLevel::Warning,
            &format!(
                "Could not record the running scrub of {} in {}: {e} — a pass killed before it ends will leave no trace there",
                marker.label,
                path.display()
            ),
        );
    }
}

/// Write a `yielded` attempt for the filesystem the pass is about to give the
/// maintenance lock up for (bd DAS-Backup-Manager-su1b.2): while this
/// process lives in this boot it reads as in progress; once it does not, as
/// `interrupted` — a pass that paused and then died is never silent. The last
/// success stays, so ageing is unchanged. A filesystem with no UUID resolved
/// has no key to file under.
fn record_yielded(
    path: &Path,
    mark: &YieldMark,
    progress: &dyn ProgressCallback,
) -> Result<(), ScrubError> {
    if mark.fsuuid.is_empty() {
        return Ok(());
    }
    let mut state = load_state_for_write(path, progress)?;
    let last_success_epoch = state
        .filesystems
        .get(&mark.fsuuid)
        .and_then(|f| f.last_success_epoch);
    state.filesystems.insert(
        mark.fsuuid.clone(),
        FsState {
            target_label: mark.label.clone(),
            mountpoint: mark.mountpoint.clone(),
            last_success_epoch,
            last_attempt: AttemptState {
                outcome: "yielded".to_string(),
                started_epoch: mark.started_epoch,
                pid: Some(std::process::id()),
                boot_id: current_boot_id(),
                yielded_to: Some(mark.holder.clone()),
                yielded_epoch: Some(mark.at_epoch),
                yields: Some(mark.yields),
                paused_secs: Some(mark.paused_secs),
                ..Default::default()
            },
        },
    );
    save_state_to(&state, path)
}

/// [`record_yielded`], with a failure logged: the pause goes on.
fn record_yielded_logged(path: &Path, mark: &YieldMark, progress: &dyn ProgressCallback) {
    if let Err(e) = record_yielded(path, mark, progress) {
        progress.on_log(
            LogLevel::Warning,
            &format!(
                "Could not record the pause of {}'s scrub in {}: {e}",
                mark.label,
                path.display()
            ),
        );
    }
}

/// File one filesystem's result the moment it is done, with a failure logged
/// (the end-of-pass write repeats it). Without this a pass killed during the
/// next target would leave this one's `running` record standing.
fn record_result_logged(path: &Path, result: &ScrubFsResult, progress: &dyn ProgressCallback) {
    if result.fsuuid.is_empty() {
        return;
    }
    let outcome = load_state_for_write(path, progress).and_then(|mut state| {
        merge_result_into_state(&mut state, result, now_epoch());
        save_state_to(&state, path)
    });
    if let Err(e) = outcome {
        progress.on_log(
            LogLevel::Warning,
            &format!(
                "Could not record the result for {} in {}: {e}",
                result.target_label,
                path.display()
            ),
        );
    }
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
    } else if pass.status == PassStatus::Paused && pass.success() {
        "PAUSED (resumes on `btrdasd scrub resume`)"
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
    if !pass.already_scrubbed.is_empty() {
        r.push_str(&format!(
            "  Already scrubbed      {} (before the pause this pass resumes)\n",
            pass.already_scrubbed.len()
        ));
    }
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
            if result.counters_unknown() {
                UNKNOWN.to_string()
            } else {
                format_bytes(result.bytes_scrubbed)
            },
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
            result.counters_summary()
        ));
    }
    for label in &pass.already_scrubbed {
        r.push_str(&format!(
            "  {label:<24} {:<9} scrubbed to the end before the pause this pass resumes; \
             its findings were in that report\n",
            "DONE"
        ));
    }

    let mut problems: Vec<String> = pass.errors.clone();
    for result in &pass.results {
        if let Some(p) = &result.paused {
            problems.push(format!(
                "{}: paused at the request of {} — not a failure; `btrdasd scrub resume` \
                 resumes it",
                result.target_label, p.by
            ));
        } else if !result.ok() {
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

    let yields: Vec<String> = pass
        .results
        .iter()
        .flat_map(|result| {
            result.yields.iter().map(|y| {
                let at = crate::caldate::local_datetime(y.at_epoch)
                    .map(|t| t.chars().take(16).collect::<String>())
                    .unwrap_or_else(|| format!("epoch {}", y.at_epoch));
                let pause = y.paused_secs.map_or_else(
                    || "pause length unknown".to_string(),
                    |p| format!("paused {}", format_duration(p)),
                );
                format!(
                    "{}: yielded at {at} to {}, {pause}",
                    result.target_label, y.holder
                )
            })
        })
        .collect();
    if !yields.is_empty() {
        r.push_str(&format!("\nYIELDS TO BACKUPS\n{thin}\n"));
        for y in &yields {
            r.push_str(&format!("  • {y}\n"));
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

    /// bd DAS-Backup-Manager-qab: flock works on a read-only descriptor, so a
    /// lock file anyone can open is a lock anyone can hold, keeping the
    /// nightly backup and scrub waiting. Root's alone: 0600, a new file and
    /// one an older version left 0644.
    #[test]
    fn a_lock_file_is_root_s_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let fresh = dir.path().join("fresh.lock");
        drop(FileLock::try_acquire(&fresh).unwrap());
        assert_eq!(mode(&fresh), 0o600, "a new lock file");
        let old = dir.path().join("old.lock");
        std::fs::write(&old, "").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(FileLock::try_acquire(&old).unwrap());
        assert_eq!(mode(&old), 0o600, "an existing 0644 lock file is narrowed");
    }
    use crate::progress::{NullProgress, TestProgress};

    /// These strings are what the operator reads in the scrub report and the
    /// journal, and each has to say which file or filesystem it is about: a
    /// message that drops its path leaves nothing to act on.
    #[test]
    fn scrub_error_messages_name_what_went_wrong_and_where() {
        let s = String::from;
        let cases = [
            (
                ScrubError::NoTargets,
                "No scrub targets: [scrub].targets is empty and no [[target]] is configured",
            ),
            (
                ScrubError::Lock {
                    path: s("/run/das-scrub.lock"),
                    detail: s("permission denied"),
                },
                "Lock '/run/das-scrub.lock': permission denied",
            ),
            (
                ScrubError::State {
                    path: s("/var/lib/das-backup/scrub-state.json"),
                    detail: s("read-only file system"),
                },
                "Scrub state '/var/lib/das-backup/scrub-state.json': read-only file system",
            ),
            (
                ScrubError::StateCorrupt {
                    path: s("/var/lib/das-backup/scrub-state.json"),
                    detail: s("expected value at line 1 column 1"),
                },
                "Scrub state '/var/lib/das-backup/scrub-state.json' is unparsable: \
                 expected value at line 1 column 1",
            ),
            (
                ScrubError::StatusMissing {
                    path: s("/var/lib/btrfs/scrub.status.abc"),
                },
                "No scrub status record at '/var/lib/btrfs/scrub.status.abc'",
            ),
            (
                ScrubError::StatusIo {
                    path: s("/var/lib/btrfs/scrub.status.abc"),
                    detail: s("permission denied"),
                },
                "Cannot read scrub status record '/var/lib/btrfs/scrub.status.abc': \
                 permission denied",
            ),
            (
                ScrubError::StatusMissingHeader,
                "Scrub status record has no 'scrub status:<n>' header",
            ),
            (
                ScrubError::StatusUnsupportedVersion { version: s("2") },
                "Unsupported scrub status format version '2'",
            ),
            (
                ScrubError::StatusUuidMismatch {
                    expected: s("5eed0016"),
                    found: s("5eed0008"),
                },
                "Scrub status record is for filesystem '5eed0008', expected '5eed0016'",
            ),
            (
                ScrubError::StatusNoDevices {
                    fsuuid: s("5eed0016"),
                },
                "Scrub status record for '5eed0016' has no device rows",
            ),
            (
                ScrubError::StatusMalformed {
                    detail: s("row 3 has 2 fields"),
                },
                "Malformed scrub status record: row 3 has 2 fields",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }

    /// Real record read from `/var/lib/btrfs/scrub.status.5eed0008-…` on the
    /// author's host (recovery-A, scrubbed 2026-07-27 — the good pass that the
    /// investigation ended with).
    const REAL_FINISHED: &str = "scrub status:1\n\
5eed0008-0000-4000-8000-000000000008:1|data_extents_scrubbed:21123358|tree_extents_scrubbed:1053852|data_bytes_scrubbed:1077538492416|tree_bytes_scrubbed:17266311168|read_errors:0|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:1824300662784|t_start:1785182081|t_resumed:0|duration:6274|canceled:0|finished:1\n";

    /// The 2026-05-24 record's shape: aborted (`canceled:0|finished:0`),
    /// nothing scrubbed, super-block errors — silently treated as healthy for
    /// 64 days. Reconstructed in the on-disk field format (that filesystem's
    /// record was overwritten by the successful 2026-07-27 scrub), from the
    /// documented `Status: aborted` / `super=3` / `Total to scrub: 0.00B`
    /// rendering. The equivalent flag shape is still live on this host in
    /// `scrub.status.953ea709-…` and `scrub.status.fd01dde9-…`, both of which
    /// this parser reports as `aborted` with zero error counters.
    const REAL_ABORTED_SUPER: &str = "scrub status:1\n\
5eed0008-0000-4000-8000-000000000008:1|data_extents_scrubbed:0|tree_extents_scrubbed:0|data_bytes_scrubbed:0|tree_bytes_scrubbed:0|read_errors:0|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:3|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:0|t_start:1779998400|t_resumed:0|duration:0|canceled:0|finished:0\n";

    /// Real two-device RAID-1 record (the primary target, both legs finished).
    const REAL_RAID1_FINISHED: &str = "scrub status:1\n\
5eed0016-0000-4000-8000-000000000016:1|data_extents_scrubbed:95871167|tree_extents_scrubbed:839307|data_bytes_scrubbed:5934387892224|tree_bytes_scrubbed:13751205888|read_errors:0|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:5960906113024|t_start:1779649863|t_resumed:0|duration:28701|canceled:0|finished:1\n\
5eed0016-0000-4000-8000-000000000016:2|data_extents_scrubbed:95871167|tree_extents_scrubbed:839220|data_bytes_scrubbed:5934387892224|tree_bytes_scrubbed:13749780480|read_errors:0|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:5961449275392|t_start:1779649863|t_resumed:0|duration:32027|canceled:0|finished:1\n";

    /// Real cancelled record (2026-05-06, filesystem 5eed0007-…): btrfs sets
    /// `canceled:1` *and* `finished:1`, plus 16 uncorrectable read errors.
    const REAL_CANCELED: &str = "scrub status:1\n\
5eed0007-0000-4000-8000-000000000007:1|data_extents_scrubbed:414|tree_extents_scrubbed:95088|data_bytes_scrubbed:8388608|tree_bytes_scrubbed:1557921792|read_errors:16|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:16|corrected_errors:0|last_physical:1601830912|t_start:1778096820|t_resumed:0|duration:9419|canceled:1|finished:1\n";

    /// Real record with a huge informational `no_csum` count on a healthy
    /// filesystem (a VM-image pool with nodatacow files).
    const REAL_NO_CSUM: &str = "scrub status:1\n\
5eed0020-0000-4000-8000-000000000020:1|data_extents_scrubbed:10233933|tree_extents_scrubbed:2076|data_bytes_scrubbed:669937827840|tree_bytes_scrubbed:34013184|read_errors:0|csum_errors:0|verify_errors:0|no_csum:163559040|csum_discards:0|super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|last_physical:1163958157312|t_start:1776373270|t_resumed:0|duration:3027|canceled:0|finished:1\n";

    // --- parsing ---------------------------------------------------------

    #[test]
    fn parses_real_finished_record() {
        let rec =
            parse_scrub_status(REAL_FINISHED, "5eed0008-0000-4000-8000-000000000008").unwrap();
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
            parse_scrub_status(REAL_ABORTED_SUPER, "5eed0008-0000-4000-8000-000000000008").unwrap();
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
        let rec = parse_scrub_status(&content, "5eed0008-0000-4000-8000-000000000008").unwrap();
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
            parse_scrub_status(REAL_CANCELED, "5eed0007-0000-4000-8000-000000000007").unwrap();
        // btrfs sets finished:1 alongside canceled:1 — canceled must win.
        assert_eq!(rec.outcome(), ScrubOutcome::Canceled);
        assert_eq!(rec.counters().read_errors, 16);
        assert_eq!(rec.counters().uncorrectable_errors, 16);
        assert!(!rec.is_clean());
    }

    #[test]
    fn raid1_record_aggregates_both_devices() {
        let rec = parse_scrub_status(REAL_RAID1_FINISHED, "5eed0016-0000-4000-8000-000000000016")
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
        let rec = parse_scrub_status(&content, "5eed0016-0000-4000-8000-000000000016").unwrap();
        assert_eq!(rec.outcome(), ScrubOutcome::Aborted);
        assert!(!rec.is_clean());
    }

    #[test]
    fn no_csum_is_informational_not_an_error() {
        let rec = parse_scrub_status(REAL_NO_CSUM, "5eed0020-0000-4000-8000-000000000020").unwrap();
        assert_eq!(rec.counters().no_csum, 163559040);
        assert_eq!(rec.counters().error_total(), 0);
        assert!(rec.is_clean());
    }

    #[test]
    fn corrected_errors_count_as_damage() {
        let content = REAL_FINISHED.replace("corrected_errors:0", "corrected_errors:7");
        let rec = parse_scrub_status(&content, "5eed0008-0000-4000-8000-000000000008").unwrap();
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
            "5eed0008-0000-4000-8000-000000000008",
            "5eed0003-0000-4000-8000-000000000003",
        );
        let err = parse_scrub_status(&nvme, "5eed0008-0000-4000-8000-000000000008")
            .expect_err("must not accept another filesystem's record");
        match err {
            ScrubError::StatusUuidMismatch { expected, found } => {
                assert_eq!(expected, "5eed0008-0000-4000-8000-000000000008");
                assert_eq!(found, "5eed0003-0000-4000-8000-000000000003");
            }
            other => panic!("expected UuidMismatch, got {other:?}"),
        }
    }

    #[test]
    fn status_record_path_is_keyed_by_uuid_only() {
        // No mount point appears anywhere in the resolved path.
        let path = status_record_path("5eed0008-0000-4000-8000-000000000008");
        let s = path.display().to_string();
        assert!(s.ends_with("/scrub.status.5eed0008-0000-4000-8000-000000000008"));
        assert!(!s.contains("/mnt/"));
    }

    #[test]
    fn read_scrub_status_reads_by_uuid_from_status_dir() {
        let dir = tempfile::tempdir().unwrap();
        let uuid = "5eed0008-0000-4000-8000-000000000008";
        fs::write(
            dir.path().join(format!("scrub.status.{uuid}")),
            REAL_FINISHED,
        )
        .unwrap();
        // Also drop a decoy record for another filesystem in the same dir.
        fs::write(
            dir.path()
                .join("scrub.status.5eed0003-0000-4000-8000-000000000003"),
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

    /// A RECOGNISED key with a non-numeric value must be refused, never read
    /// as 0. Before bd DAS-Backup-Manager-8wx every counter fell back to 0 on a
    /// parse failure, so a truncated or corrupted status record parsed cleanly
    /// and `is_clean()` returned TRUE — damage reported as a good scrub, no
    /// FAILURE email, health green.
    #[test]
    fn rejects_non_numeric_value_on_a_recognised_counter() {
        let corrupt = "scrub status:1\nabc:1|finished:1|canceled:0|uncorrectable_errors:1?7|t_start:100|duration:5\n";
        let err = parse_scrub_status(corrupt, "abc")
            .expect_err("a corrupt error counter must not parse as a clean scrub");
        match err {
            ScrubError::StatusMalformed { detail } => {
                assert!(
                    detail.contains("uncorrectable_errors") && detail.contains("1?7"),
                    "detail should name the offending field and value, got: {detail}"
                );
            }
            other => panic!("expected StatusMalformed, got {other:?}"),
        }

        // Positive control on the same record: with a parseable value it still
        // parses and the count survives, so the guard cannot be passing by
        // refusing everything.
        let good = corrupt.replace("uncorrectable_errors:1?7", "uncorrectable_errors:17");
        let rec = parse_scrub_status(&good, "abc").expect("a well-formed record must still parse");
        assert_eq!(rec.counters().uncorrectable_errors, 17);
        assert!(
            !rec.is_clean(),
            "17 uncorrectable errors is not a clean scrub"
        );
    }

    /// The counterpart guard: a corrupt `finished`/`canceled` flag must not be
    /// silently read as 0 either, because 0/0 is "still running", not "unknown".
    #[test]
    fn rejects_non_numeric_value_on_an_outcome_flag() {
        let corrupt = "scrub status:1\nabc:1|finished:yes|canceled:0|t_start:100|duration:5\n";
        let err = parse_scrub_status(corrupt, "abc").expect_err("'finished:yes' must not parse");
        assert!(matches!(err, ScrubError::StatusMalformed { .. }), "{err:?}");
    }

    // --- resumed scrubs / freshness anchor --------------------------------

    #[test]
    fn freshness_anchor_uses_the_later_of_start_and_resume() {
        // A resumed scrub keeps its original t_start; anchoring on t_start
        // alone would call a fresh resume "stale".
        let content = REAL_FINISHED.replace("t_resumed:0", "t_resumed:1785200000");
        let rec = parse_scrub_status(&content, "5eed0008-0000-4000-8000-000000000008").unwrap();
        assert_eq!(rec.t_start(), 1785182081);
        assert_eq!(rec.devices[0].t_resumed, 1785200000);
        assert_eq!(rec.last_activity_start(), 1785200000);
    }

    #[test]
    fn freshness_anchor_falls_back_to_t_start_when_never_resumed() {
        let rec =
            parse_scrub_status(REAL_FINISHED, "5eed0008-0000-4000-8000-000000000008").unwrap();
        assert_eq!(rec.last_activity_start(), rec.t_start());
    }

    // --- resume-or-start decision (C1) ------------------------------------

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
        let uuid = "5eed0008-0000-4000-8000-000000000008";
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
    /// resume — the engine only resumes on positive evidence that nothing runs.
    #[test]
    fn aborted_prior_record_does_not_resume_when_liveness_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let uuid = "5eed0008-0000-4000-8000-000000000008";
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
                "must not resume when liveness cannot be determined"
            );
        });
    }

    // --- pure decision, all four combinations -----------------------------

    /// bd DAS-Backup-Manager-su1b.1/.5: every scrub argv carries the chosen
    /// class's level-0 priority — btrfs's workers reset their own priority, so
    /// the argv is the only place it can be set — and keeps the order and
    /// flags it had, the mountpoint last.
    #[test]
    fn scrub_argv_start_carries_priority_and_keeps_its_shape() {
        assert_eq!(
            btrfs_scrub_args(ScrubInvocation::Start, ScrubIoClass::BestEffort, "/mnt/x"),
            ["scrub", "start", "-B", "-c", "2", "-n", "0", "/mnt/x"]
        );
        assert_eq!(
            btrfs_scrub_args(ScrubInvocation::Start, ScrubIoClass::Realtime, "/mnt/x"),
            ["scrub", "start", "-B", "-c", "1", "-n", "0", "/mnt/x"]
        );
    }

    #[test]
    fn scrub_argv_resume_carries_priority_and_keeps_its_shape() {
        assert_eq!(
            btrfs_scrub_args(ScrubInvocation::Resume, ScrubIoClass::BestEffort, "/mnt/x"),
            ["scrub", "resume", "-B", "-c", "2", "-n", "0", "/mnt/x"]
        );
        assert_eq!(
            btrfs_scrub_args(ScrubInvocation::Resume, ScrubIoClass::Realtime, "/mnt/x"),
            ["scrub", "resume", "-B", "-c", "1", "-n", "0", "/mnt/x"]
        );
    }

    #[test]
    fn scrub_argv_forced_start_carries_priority_and_keeps_its_shape() {
        assert_eq!(
            btrfs_scrub_args(
                ScrubInvocation::ForcedStart,
                ScrubIoClass::BestEffort,
                "/mnt/x"
            ),
            ["scrub", "start", "-B", "-f", "-c", "2", "-n", "0", "/mnt/x"]
        );
        assert_eq!(
            btrfs_scrub_args(
                ScrubInvocation::ForcedStart,
                ScrubIoClass::Realtime,
                "/mnt/x"
            ),
            ["scrub", "start", "-B", "-f", "-c", "1", "-n", "0", "/mnt/x"]
        );
    }

    // --- the I/O class rule (bd DAS-Backup-Manager-su1b.5) ------------------

    const FS: &str = "5eed0016-0000-4000-8000-000000000016";

    /// One member of a fake filesystem: the entry name under
    /// `fs/btrfs/<uuid>/devices/`, the disk it lives on, the disk's
    /// `queue/scheduler` text (None: no file) and its `prio_aging_expire`
    /// (None: no file).
    struct Member<'a> {
        entry: &'a str,
        disk: &'a str,
        scheduler: Option<&'a str>,
        aging: Option<&'a str>,
    }

    fn member<'a>(
        entry: &'a str,
        disk: &'a str,
        scheduler: Option<&'a str>,
        aging: Option<&'a str>,
    ) -> Member<'a> {
        Member {
            entry,
            disk,
            scheduler,
            aging,
        }
    }

    const MQ: &str = "none [mq-deadline] kyber adios bfq\n";
    const BFQ: &str = "none mq-deadline kyber adios [bfq]\n";

    /// A sysfs tree shaped as the kernel's: each entry under
    /// `fs/btrfs/<uuid>/devices/` is a symlink into
    /// `devices/.../block/<disk>[/<partition>]`, a partition directory holds a
    /// `partition` file, and the disk's `queue/` holds the scheduler files.
    fn fake_sysfs(members: &[Member<'_>]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let devices = root.path().join("fs/btrfs").join(FS).join("devices");
        fs::create_dir_all(&devices).unwrap();
        for m in members {
            let disk = root.path().join("devices/pci0/host0/block").join(m.disk);
            fs::create_dir_all(disk.join("queue/iosched")).unwrap();
            if let Some(s) = m.scheduler {
                fs::write(disk.join("queue/scheduler"), s).unwrap();
            }
            if let Some(a) = m.aging {
                fs::write(disk.join("queue/iosched/prio_aging_expire"), a).unwrap();
            }
            let target = if m.entry == m.disk {
                disk
            } else {
                let part = disk.join(m.entry);
                fs::create_dir_all(&part).unwrap();
                fs::write(part.join("partition"), "1\n").unwrap();
                part
            };
            std::os::unix::fs::symlink(&target, devices.join(m.entry)).unwrap();
        }
        root
    }

    #[test]
    fn every_member_on_mq_deadline_with_aging_runs_realtime() {
        let root = fake_sysfs(&[
            member("sdi1", "sdi", Some(MQ), Some("10000\n")),
            member("sdj1", "sdj", Some(MQ), Some("10000\n")),
        ]);
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::Realtime, "{choice:?}");
        assert_eq!(
            choice.why,
            "every device runs mq-deadline (sdi, sdj; starvation bounded at 10 s)"
        );
        assert_eq!(
            choice.log_line("/mnt/t"),
            "Scrub priority on /mnt/t: realtime I/O, level 0 (-c 1 -n 0): every device \
             runs mq-deadline (sdi, sdj; starvation bounded at 10 s)"
        );
    }

    /// The bound named is the longest any member allows, in ms when it is not
    /// whole seconds.
    #[test]
    fn realtime_names_the_longest_aging_bound() {
        let root = fake_sysfs(&[
            member("sda", "sda", Some(MQ), Some("2500")),
            member("sdb", "sdb", Some(MQ), Some("1000")),
        ]);
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::Realtime);
        assert!(
            choice.why.ends_with("bounded at 2500 ms)"),
            "{}",
            choice.why
        );
    }

    /// A partition entry resolves to its disk: the scheduler is read from the
    /// disk's queue, and the disk's name is the one reported.
    #[test]
    fn a_partition_entry_is_judged_by_its_disk() {
        let root = fake_sysfs(&[member("sdi1", "sdi", Some(BFQ), None)]);
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::BestEffort);
        assert_eq!(choice.why, "sdi runs bfq");
    }

    #[test]
    fn any_member_on_bfq_keeps_best_effort() {
        let root = fake_sysfs(&[
            member("sdi1", "sdi", Some(MQ), Some("10000")),
            member("sdj1", "sdj", Some(BFQ), None),
        ]);
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::BestEffort);
        assert_eq!(choice.why, "sdj runs bfq");
        assert_eq!(
            choice.log_line("/mnt/t"),
            "Scrub priority on /mnt/t: best-effort I/O, level 0 (-c 2 -n 0): sdj runs bfq"
        );
    }

    #[test]
    fn mq_deadline_without_prio_aging_expire_keeps_best_effort() {
        let root = fake_sysfs(&[
            member("sdi1", "sdi", Some(MQ), Some("10000")),
            member("sdj1", "sdj", Some(MQ), None),
        ]);
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::BestEffort);
        assert!(
            choice
                .why
                .starts_with("sdj runs mq-deadline without prio_aging_expire"),
            "{}",
            choice.why
        );
    }

    /// An aging value that is not a positive number of ms is no bound.
    #[test]
    fn an_unparsable_or_zero_aging_value_keeps_best_effort() {
        for bad in ["", "ten", "0", "-1"] {
            let root = fake_sysfs(&[member("sdi1", "sdi", Some(MQ), Some(bad))]);
            let choice = choose_scrub_io_class(root.path(), FS);
            assert_eq!(choice.class, ScrubIoClass::BestEffort, "aging {bad:?}");
            assert!(choice.why.starts_with("sdi: "), "{}", choice.why);
        }
    }

    #[test]
    fn unreadable_or_unbracketed_scheduler_keeps_best_effort() {
        let root = fake_sysfs(&[member("sdi1", "sdi", None, Some("10000"))]);
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::BestEffort);
        assert!(
            choice.why.starts_with("sdi: cannot read queue/scheduler"),
            "{}",
            choice.why
        );

        let root = fake_sysfs(&[member("sdi1", "sdi", Some("mq-deadline\n"), Some("10000"))]);
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::BestEffort);
        assert!(
            choice.why.starts_with("sdi: no active scheduler"),
            "{}",
            choice.why
        );
    }

    #[test]
    fn missing_or_empty_devices_dir_keeps_best_effort() {
        let root = tempfile::tempdir().unwrap();
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::BestEffort);
        assert!(choice.why.starts_with("cannot read "), "{}", choice.why);

        let root = fake_sysfs(&[]);
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::BestEffort);
        assert!(choice.why.starts_with("no member device"), "{}", choice.why);
    }

    /// A dangling member link (the device went away) is unreadable, not
    /// skipped: the rest being mq-deadline must not make it realtime.
    #[test]
    fn a_dangling_member_link_keeps_best_effort() {
        let root = fake_sysfs(&[member("sdi1", "sdi", Some(MQ), Some("10000"))]);
        let devices = root.path().join("fs/btrfs").join(FS).join("devices");
        std::os::unix::fs::symlink(root.path().join("devices/gone"), devices.join("sdz1")).unwrap();
        let choice = choose_scrub_io_class(root.path(), FS);
        assert_eq!(choice.class, ScrubIoClass::BestEffort);
        assert!(
            choice.why.starts_with("sdz1: cannot resolve"),
            "{}",
            choice.why
        );
    }

    /// A filesystem UUID that is not one cannot walk out of `fs/btrfs`.
    #[test]
    fn a_malformed_fsuuid_keeps_best_effort() {
        let root = fake_sysfs(&[member("sdi1", "sdi", Some(MQ), Some("10000"))]);
        for bad in [
            "",
            "../..",
            "a/b",
            "5eed0016-0000-4000-8000-000000000016/..",
        ] {
            let choice = choose_scrub_io_class(root.path(), bad);
            assert_eq!(choice.class, ScrubIoClass::BestEffort, "{bad:?}");
            assert!(
                choice.why.starts_with("not a filesystem UUID"),
                "{}",
                choice.why
            );
        }
    }

    /// `decide_from` is the pure seam behind `decide_scrub_start_mode`. These
    /// exercise every (outcome × liveness) combination that matters — the case
    /// a real mount cannot produce in a unit test (aborted + NotRunning →
    /// Resume) is the one that actually distinguishes the feature, so it is
    /// asserted here rather than left to the root-only loopback tests.
    #[test]
    fn decide_from_resumes_only_on_aborted_and_not_running() {
        let u = "abcd";
        // The one case that resumes.
        assert!(matches!(
            decide_from(u, ScrubOutcome::Aborted, LiveScrubState::NotRunning),
            ScrubStartMode::Resume { .. }
        ));
        // Aborted but a scrub is live, or liveness unknown: never resume.
        assert_eq!(
            decide_from(u, ScrubOutcome::Aborted, LiveScrubState::Running),
            ScrubStartMode::Normal
        );
        assert_eq!(
            decide_from(u, ScrubOutcome::Aborted, LiveScrubState::Unknown),
            ScrubStartMode::Normal
        );
        // A finished or canceled record has no resumable position, regardless
        // of liveness — this is the `!= Aborted` guard, and it must hold even
        // when nothing is running (the case that would otherwise resume).
        for live in [
            LiveScrubState::NotRunning,
            LiveScrubState::Running,
            LiveScrubState::Unknown,
        ] {
            assert_eq!(
                decide_from(u, ScrubOutcome::Finished, live),
                ScrubStartMode::Normal,
                "a finished record must never resume"
            );
            assert_eq!(
                decide_from(u, ScrubOutcome::Canceled, live),
                ScrubStartMode::Normal,
                "a deliberately-canceled record must never auto-resume"
            );
        }
    }

    // --- which argv each path runs ------------------------------------------

    /// Run `run_btrfs_scrub_with` for `mode`, answering each spawn from
    /// `answers` in order; returns the outcome and every argv asked for.
    fn drive_scrub(
        mode: ScrubStartMode,
        answers: Vec<ScrubRunOutcome>,
    ) -> (ScrubRunOutcome, Vec<Vec<String>>) {
        let io = ScrubIoChoice::best_effort("test".into());
        let (outcome, calls, _) = drive_scrub_at(&io, mode, answers);
        (outcome, calls)
    }

    /// [`drive_scrub`] at the I/O class `io`, also returning the log lines.
    fn drive_scrub_at(
        io: &ScrubIoChoice,
        mode: ScrubStartMode,
        answers: Vec<ScrubRunOutcome>,
    ) -> (ScrubRunOutcome, Vec<Vec<String>>, Vec<(LogLevel, String)>) {
        let calls = std::cell::RefCell::new(Vec::<Vec<String>>::new());
        let answers = std::cell::RefCell::new(answers.into_iter());
        let progress = Steps::default();
        let outcome = run_btrfs_scrub_with(
            mode,
            io,
            "/mnt/x",
            &|args| {
                calls.borrow_mut().push(args.to_vec());
                answers
                    .borrow_mut()
                    .next()
                    .expect("more spawns than answers")
            },
            &progress,
        );
        let logs = progress.logs.into_inner().unwrap();
        (outcome, calls.into_inner(), logs)
    }

    fn argv(invocation: ScrubInvocation) -> Vec<String> {
        btrfs_scrub_args(invocation, ScrubIoClass::BestEffort, "/mnt/x")
    }

    #[test]
    fn normal_mode_runs_one_plain_start() {
        let (outcome, calls) = drive_scrub(
            ScrubStartMode::Normal,
            vec![ScrubRunOutcome::Launched(Ok(()))],
        );
        assert!(matches!(outcome, ScrubRunOutcome::Launched(Ok(()))));
        assert_eq!(calls, vec![argv(ScrubInvocation::Start)]);
    }

    #[test]
    fn resume_mode_runs_one_resume_and_stops_when_it_works() {
        let (outcome, calls) = drive_scrub(
            ScrubStartMode::Resume { reason: "r".into() },
            vec![ScrubRunOutcome::Launched(Ok(()))],
        );
        assert!(matches!(outcome, ScrubRunOutcome::Launched(Ok(()))));
        assert_eq!(calls, vec![argv(ScrubInvocation::Resume)]);
    }

    #[test]
    fn resume_with_nothing_to_resume_falls_back_to_a_forced_start() {
        let (outcome, calls) = drive_scrub(
            ScrubStartMode::Resume { reason: "r".into() },
            vec![
                ScrubRunOutcome::Launched(Err(
                    "btrfs scrub resume exited 2: nothing to resume for /mnt/x".into(),
                )),
                ScrubRunOutcome::Launched(Ok(())),
            ],
        );
        assert!(matches!(outcome, ScrubRunOutcome::Launched(Ok(()))));
        assert_eq!(
            calls,
            vec![
                argv(ScrubInvocation::Resume),
                argv(ScrubInvocation::ForcedStart)
            ]
        );
    }

    #[test]
    fn a_real_resume_failure_is_returned_unchanged_without_a_second_run() {
        let (outcome, calls) = drive_scrub(
            ScrubStartMode::Resume { reason: "r".into() },
            vec![ScrubRunOutcome::Launched(Err(
                "btrfs scrub resume exited 1: EIO".into(),
            ))],
        );
        assert!(
            matches!(&outcome, ScrubRunOutcome::Launched(Err(m)) if m == "btrfs scrub resume exited 1: EIO"),
        );
        assert_eq!(calls, vec![argv(ScrubInvocation::Resume)]);
    }

    /// Every argv any path asks for carries the priority flags and ends with
    /// the mountpoint — the wiring cannot go back to a literal argv unseen.
    #[test]
    fn every_path_runs_the_priority_argv() {
        let nothing =
            || ScrubRunOutcome::Launched(Err("resume exited 2: nothing to resume".into()));
        for (class, flags) in [
            (ScrubIoClass::BestEffort, ["-c", "2", "-n", "0"]),
            (ScrubIoClass::Realtime, ["-c", "1", "-n", "0"]),
        ] {
            let io = ScrubIoChoice {
                class,
                why: "w".into(),
            };
            let (_, calls, logs) = drive_scrub_at(
                &io,
                ScrubStartMode::Resume { reason: "r".into() },
                vec![nothing(), ScrubRunOutcome::Launched(Ok(()))],
            );
            let (_, normal, _) = drive_scrub_at(
                &io,
                ScrubStartMode::Normal,
                vec![ScrubRunOutcome::Launched(Ok(()))],
            );
            for call in calls.iter().chain(normal.iter()) {
                let at = call.iter().position(|a| a == "-c").expect("-c present");
                assert_eq!(call[at..at + 4], flags, "{call:?}");
                assert_eq!(call.last().map(String::as_str), Some("/mnt/x"));
            }
            assert_eq!(calls.len() + normal.len(), 3);
            // One priority line per filesystem, before the run, even when the
            // resume falls back to a forced start.
            let lines: Vec<_> = logs
                .iter()
                .filter(|(_, m)| m.starts_with("Scrub priority on /mnt/x: "))
                .collect();
            assert_eq!(lines.len(), 1, "{logs:?}");
            assert_eq!(lines[0].1, io.log_line("/mnt/x"));
        }
    }

    // --- resume fallback predicate ----------------------------------------

    /// The fallback to a fresh `start -f` fires ONLY on the kernel's
    /// "nothing to resume" message, and NOT on any other launched failure —
    /// otherwise a genuinely broken scrub would be masked by silently
    /// restarting it from zero. The message text is the real one observed
    /// 2026-09-09; the runner formats it as `btrfs scrub resume -B -c 2 -n 0
    /// <mnt> exited 2: ERROR: ... nothing to resume for <mnt>` (the full argv,
    /// priority flags included, since bd DAS-Backup-Manager-su1b.1).
    #[test]
    fn resume_fallback_fires_only_on_nothing_to_resume() {
        let nothing = ScrubRunOutcome::Launched(Err(
            "btrfs scrub resume -B -c 2 -n 0 /mnt/x exited 2: ERROR: there is no scrub \
             running, nothing to resume for /mnt/x"
                .to_string(),
        ));
        assert!(
            resume_found_nothing(&nothing),
            "the 'nothing to resume' case must trigger the fresh-start fallback"
        );

        // A real failure (I/O error, unreadable device) must NOT be retried as
        // a fresh start — that would discard the diagnosis and re-scrub blind.
        let real_failure = ScrubRunOutcome::Launched(Err(
            "btrfs scrub resume -B -c 2 -n 0 /mnt/x exited 1: ERROR: cannot read status"
                .to_string(),
        ));
        assert!(
            !resume_found_nothing(&real_failure),
            "a genuine resume failure must surface, not silently restart"
        );

        // A clean resume and a spawn failure are likewise not the fallback case.
        assert!(!resume_found_nothing(&ScrubRunOutcome::Launched(Ok(()))));
        assert!(!resume_found_nothing(&ScrubRunOutcome::SpawnFailed(
            "no such binary".to_string()
        )));
    }

    /// Verbatim `btrfs scrub status` output captured from the v7.1 loopback
    /// rig on 2026-08-01, one sample per state the engine must distinguish.
    #[test]
    fn live_scrub_state_parses_real_status_output() {
        let running = "UUID:             5eed0013-0000-4000-8000-000000000013\n\
Scrub started:    Sat Aug  1 15:44:08 2026\n\
Status:           running\n\
Duration:         0:00:03\n\
Bytes scrubbed:   257.12MiB  (64.08%)\n";
        assert_eq!(parse_scrub_status_output(running), LiveScrubState::Running);

        let finished = "UUID:             5eed0013-0000-4000-8000-000000000013\n\
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
        let no_stats = "UUID:             5eed0013-0000-4000-8000-000000000013\n\
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
            esp_partuuid: None,
        });
        config
    }

    #[test]
    fn resolve_fsuuid_prefers_configured_mount_uuid() {
        // Serial resolution would need the physical drive; mount_uuid must win
        // without ever touching the block layer.
        let config = config_with_target(
            "primary-22tb",
            Some("5eed0016-0000-4000-8000-000000000016"),
            vec!["SERIAL11", "SERIAL12"],
        );
        assert_eq!(
            resolve_target_fsuuid(&config, "primary-22tb").unwrap(),
            "5eed0016-0000-4000-8000-000000000016"
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

    /// Let go at once even while a copy of its descriptor lives on, as a fork
    /// of this process holds every descriptor until its exec: closing alone
    /// would leave the lock held by the copy (bd DAS-Backup-Manager-eu0). The
    /// converse, a plain close, shows the copy would hold it.
    #[test]
    fn file_lock_is_released_on_drop_while_a_copy_of_its_descriptor_lives() {
        use std::os::fd::AsFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let lock = FileLock::try_acquire(&path).unwrap().unwrap();
        let copy = lock.file.as_fd().try_clone_to_owned().unwrap();
        drop(lock);
        assert!(
            FileLock::try_acquire(&path).unwrap().is_some(),
            "released for good, whatever copies of its descriptor live"
        );
        drop(copy);

        let plain = File::open(&path).unwrap();
        // SAFETY: flock on a descriptor `plain` owns.
        assert_eq!(
            unsafe { libc::flock(plain.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let copy = plain.as_fd().try_clone_to_owned().unwrap();
        drop(plain);
        assert!(
            FileLock::try_acquire(&path).unwrap().is_none(),
            "closed but not unlocked: the copy holds the lock"
        );
        drop(copy);
        // Released by the last close, not at once: a child another test
        // thread forks inherits this description and holds the lock until it
        // execs (bd q9bx). The earlier release is immune: drop unlocks.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while FileLock::try_acquire(&path).unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "still held 10 s after the last copy of its descriptor closed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A lock file that cannot be opened says why, and says root is needed
    /// only when root is what is missing.
    #[test]
    fn a_lock_file_that_cannot_be_opened_names_root_only_when_denied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "").unwrap();
        // Not a directory: root would fail the same way.
        let err = FileLock::try_acquire(dir.path().join("file").join("lock"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot open lock file: "), "{err}");
        assert!(!err.contains("must run as root"), "{err}");

        // SAFETY: geteuid() is always safe. Root is never denied here.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping the denied half: running as root");
            return;
        }
        let shut = dir.path().join("shut");
        fs::create_dir(&shut).unwrap();
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o500)).unwrap();
        let err = FileLock::try_acquire(shut.join("lock"))
            .unwrap_err()
            .to_string();
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            err.contains("— this command must run as root (sudo)"),
            "{err}"
        );
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
        let result = locks_now(&singleton, &maintenance, &progress);
        assert!(
            matches!(result, ScrubLockAttempt::Held(_)),
            "must skip when singleton is held: {result:?}"
        );
        assert!(
            !maintenance.exists(),
            "maintenance lock must not be opened when the singleton is unavailable"
        );

        drop(held);
        let ScrubLockAttempt::Taken(locks) = locks_now(&singleton, &maintenance, &progress) else {
            panic!("locks available");
        };
        assert!(maintenance.exists());
        let (s, m) = locks.paths();
        assert_eq!(s, singleton.as_path());
        assert_eq!(m, maintenance.as_path());
        assert_eq!(locks.maintenance().unwrap().path(), maintenance.as_path());
        assert_eq!(
            fs::read_to_string(&maintenance).unwrap(),
            format!("btrdasd scrub run pid {}\n", std::process::id()),
            "the scrub is recorded as the maintenance lock's holder"
        );

        // Both locks are held: a second pass skips.
        assert!(matches!(
            locks_now(&singleton, &maintenance, &progress),
            ScrubLockAttempt::Held(_)
        ));

        drop(locks);
        assert!(
            matches!(
                locks_now(&singleton, &maintenance, &progress),
                ScrubLockAttempt::Taken(_)
            ),
            "both locks must be released on drop"
        );
    }

    /// [`acquire_locks_at`] with no wait for setup, for the tests that are
    /// not about that wait.
    fn locks_now(
        singleton: &Path,
        maintenance: &Path,
        progress: &dyn ProgressCallback,
    ) -> ScrubLockAttempt {
        acquire_locks_at(
            singleton,
            maintenance,
            &maintenance.with_file_name("claim.lock"),
            progress,
            Duration::ZERO,
            Duration::from_millis(10),
        )
        .unwrap()
    }

    /// su1b.2 A4 fix round: a pass records itself in its singleton, as every
    /// holder of a lock here does, and empties the record before it lets go.
    #[test]
    fn a_pass_records_itself_in_its_singleton_and_empties_it_on_release() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        let maintenance = dir.path().join("das-maintenance.lock");
        let ScrubLockAttempt::Taken(locks) = locks_now(&singleton, &maintenance, &NullProgress)
        else {
            panic!("free locks are taken");
        };
        assert_eq!(
            crate::maintenance::holder_of(&singleton),
            format!("btrdasd scrub run pid {}", std::process::id())
        );
        drop(locks);
        assert_eq!(fs::read_to_string(&singleton).unwrap(), "");
    }

    /// A second pass that meets a pass skips at once and names it.
    #[test]
    fn a_pass_meeting_another_pass_skips_at_once_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        let maintenance = dir.path().join("das-maintenance.lock");
        let other = crate::backup::SingletonHeld::try_acquire_at(&singleton, "btrdasd scrub run")
            .unwrap()
            .unwrap();
        let started = Instant::now();
        let attempt = acquire_locks_at(
            &singleton,
            &maintenance,
            &maintenance.with_file_name("claim.lock"),
            &NullProgress,
            Duration::from_secs(30),
            Duration::from_millis(10),
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no wait for another pass"
        );
        let holder = format!("btrdasd scrub run pid {}", std::process::id());
        match &attempt {
            ScrubLockAttempt::Held(h) => assert_eq!(h, &holder),
            other => panic!("skips: {other:?}"),
        }
        let (level, line, _) = skipped_lines(&attempt);
        assert_eq!(level, LogLevel::Info);
        assert_eq!(
            line,
            format!("{SCRUB_LOCK_PATH} is held by {holder} — skipping this invocation")
        );
        drop(other);
    }

    /// A singleton held with no record (or one that cannot be read) is not
    /// asserted to be a scrub: the line says the record names nobody.
    #[test]
    fn a_singleton_held_with_no_record_skips_saying_so() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        let maintenance = dir.path().join("das-maintenance.lock");
        let held = FileLock::try_acquire(&singleton).unwrap().unwrap();
        let attempt = locks_now(&singleton, &maintenance, &NullProgress);
        let (level, line, summary) = skipped_lines(&attempt);
        assert_eq!(level, LogLevel::Info);
        assert_eq!(
            line,
            format!(
                "{SCRUB_LOCK_PATH} is held by an unknown holder (its record is empty or \
                 unreadable) — skipping this invocation"
            )
        );
        assert!(!summary.contains("another scrub"), "{summary}");
        drop(held);
    }

    /// Held by `btrdasd setup`: the pass waits, saying for whom, and runs once
    /// setup lets go.
    #[test]
    fn a_pass_waits_for_setup_and_runs_once_it_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        let maintenance = dir.path().join("das-maintenance.lock");
        let setup =
            crate::backup::SingletonHeld::try_acquire_at(&singleton, "btrdasd setup --upgrade")
                .unwrap()
                .unwrap();
        let progress = TestProgress::new();
        let attempt = std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                drop(setup);
            });
            acquire_locks_at(
                &singleton,
                &maintenance,
                &maintenance.with_file_name("claim.lock"),
                &progress,
                Duration::from_secs(30),
                Duration::from_millis(20),
            )
            .unwrap()
        });
        assert!(
            matches!(attempt, ScrubLockAttempt::Taken(_)),
            "ran once setup let go: {attempt:?}"
        );
        let logs = progress.logs.lock().unwrap().clone();
        let holder = format!("btrdasd setup --upgrade pid {}", std::process::id());
        assert!(
            logs.iter().any(|(level, line)| *level == LogLevel::Info
                && line.contains(&holder)
                && line.contains("waiting")),
            "says whom it waits for: {logs:?}"
        );
    }

    /// CI mutants on 12114f7d: the words a wait is said in, each time branch.
    #[test]
    fn a_wait_is_said_in_minutes_seconds_or_milliseconds() {
        assert_eq!(wait_words(Duration::from_secs(SETUP_WAIT_SECS)), "15 min");
        assert_eq!(wait_words(Duration::from_secs(60)), "1 min");
        assert_eq!(wait_words(Duration::from_secs(90)), "90 s");
        assert_eq!(wait_words(Duration::from_secs(59)), "59 s");
        assert_eq!(wait_words(Duration::from_secs(1)), "1 s");
        assert_eq!(wait_words(Duration::from_millis(200)), "200 ms");
        assert_eq!(wait_words(Duration::ZERO), "0 ms");
    }

    /// The exact line a pass logs when setup holds its singleton.
    #[test]
    fn the_wait_for_setup_is_said_in_these_words() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        let maintenance = dir.path().join("das-maintenance.lock");
        let setup = crate::backup::SingletonHeld::try_acquire_at(&singleton, "btrdasd setup")
            .unwrap()
            .unwrap();
        let progress = TestProgress::new();
        let _ = acquire_locks_at(
            &singleton,
            &maintenance,
            &maintenance.with_file_name("claim.lock"),
            &progress,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .unwrap();
        drop(setup);
        let logs = progress.logs.lock().unwrap().clone();
        assert_eq!(
            logs.first(),
            Some(&(
                LogLevel::Info,
                format!(
                    "{} is held by btrdasd setup pid {}, which is writing the installed files — \
                     waiting up to 50 ms for it",
                    singleton.display(),
                    std::process::id()
                )
            )),
            "{logs:?}"
        );
    }

    /// CI mutant on 12114f7d (`holder_is_setup(&next)` → `true`): while a pass
    /// waits for setup, setup lets go and ANOTHER PASS takes the singleton
    /// first. The waiting pass skips at once, naming that pass — it does not
    /// sit out the rest of its wait as if setup still held it.
    #[test]
    fn a_pass_waiting_for_setup_skips_at_once_when_another_pass_takes_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        let maintenance = dir.path().join("das-maintenance.lock");
        let setup = crate::backup::SingletonHeld::try_acquire_at(&singleton, "btrdasd setup")
            .unwrap()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let path = singleton.clone();
        let other = std::thread::spawn(move || {
            // Setup lets go, and another pass takes the lock before the
            // waiter looks again (it looks every 200 ms).
            std::thread::sleep(Duration::from_millis(50));
            drop(setup);
            let pass = crate::backup::SingletonHeld::try_acquire_at(&path, SCRUB_JOB)
                .unwrap()
                .expect("free once setup let go");
            rx.recv().unwrap();
            drop(pass);
        });
        let started = Instant::now();
        let attempt = acquire_locks_at(
            &singleton,
            &maintenance,
            &maintenance.with_file_name("claim.lock"),
            &NullProgress,
            Duration::from_secs(30),
            Duration::from_millis(200),
        )
        .unwrap();
        let waited = started.elapsed();
        tx.send(()).unwrap();
        other.join().unwrap();
        match attempt {
            ScrubLockAttempt::Held(holder) => {
                assert_eq!(holder, format!("{SCRUB_JOB} pid {}", std::process::id()));
            }
            other => panic!("another pass holds it: skip at once, not {other:?}"),
        }
        assert!(
            waited < Duration::from_secs(2),
            "it sat out the wait: {waited:?}"
        );
        assert!(
            !maintenance.exists(),
            "nothing past the singleton was tried"
        );
    }

    /// CI mutants on 12114f7d (look_at_singleton's `attempt == 1 || holder
    /// != UNKNOWN_HOLDER`): a holder that has emptied its record is letting
    /// go, so an empty record is looked at once more after the grace — and a
    /// lock free by then is taken, never a skip of a pass that could run.
    #[test]
    fn an_empty_record_is_looked_at_again_and_a_lock_let_go_meanwhile_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        // Held with no record, let go 30 ms later: within the 100 ms grace.
        let silent = FileLock::try_acquire(&singleton).unwrap().unwrap();
        let letting_go = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(silent);
        });
        let look = look_at_singleton(&singleton, Duration::from_millis(100)).unwrap();
        letting_go.join().unwrap();
        assert!(
            matches!(look, SingletonLook::Taken(_)),
            "an empty record is looked at again, and the lock is free by then"
        );
    }

    /// The other side of the same line: a holder that named itself is
    /// reported at the first look, without the grace.
    #[test]
    fn a_named_holder_is_reported_at_the_first_look() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        let _pass = crate::backup::SingletonHeld::try_acquire_at(&singleton, SCRUB_JOB)
            .unwrap()
            .unwrap();
        let started = Instant::now();
        let look = look_at_singleton(&singleton, Duration::from_secs(2)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no grace for a named holder"
        );
        match look {
            SingletonLook::HeldBy(h) => {
                assert_eq!(h, format!("{SCRUB_JOB} pid {}", std::process::id()));
            }
            SingletonLook::Taken(_) => panic!("held"),
        }
    }

    /// Setup still holding once the wait is over: the pass did not run, and
    /// says so at WARN, naming setup; it is still a skip (exit 0).
    #[test]
    fn a_pass_that_outwaits_its_wait_for_setup_skips_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("das-scrub.lock");
        let maintenance = dir.path().join("das-maintenance.lock");
        let setup = crate::backup::SingletonHeld::try_acquire_at(&singleton, "btrdasd setup")
            .unwrap()
            .unwrap();
        let started = Instant::now();
        let attempt = acquire_locks_at(
            &singleton,
            &maintenance,
            &maintenance.with_file_name("claim.lock"),
            &NullProgress,
            Duration::from_millis(200),
            Duration::from_millis(20),
        )
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(200), "it waited");
        let holder = format!("btrdasd setup pid {}", std::process::id());
        match &attempt {
            ScrubLockAttempt::SetupOutlasted(h) => assert_eq!(h, &holder),
            other => panic!("setup outlasted the wait: {other:?}"),
        }
        assert!(
            !maintenance.exists(),
            "nothing past the singleton was tried"
        );
        let (level, line, summary) = skipped_lines(&attempt);
        assert_eq!(level, LogLevel::Warning);
        assert!(line.contains(&holder), "{line}");
        assert!(line.contains("did NOT run"), "{line}");
        assert!(summary.contains("did NOT run"), "{summary}");
        drop(setup);
    }

    #[test]
    fn blocking_acquire_returns_immediately_when_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free.lock");
        let progress = TestProgress::new();
        let started = Instant::now();
        let lock = FileLock::acquire_blocking(&path, &progress, &|| "waiting".into()).unwrap();
        assert!(started.elapsed() < Duration::from_secs(LOCK_WAIT_ANNOUNCE_SECS));
        assert_eq!(lock.path(), path.as_path());
        // No wait announcement when there was no wait.
        assert!(progress.logs.lock().unwrap().is_empty());
    }

    // --- the pass loop -----------------------------------------------------

    /// Records progress steps as well as stages and logs.
    #[derive(Default)]
    struct Steps {
        stages: std::sync::Mutex<Vec<(String, u64)>>,
        steps: std::sync::Mutex<Vec<(u64, u64, String)>>,
        logs: std::sync::Mutex<Vec<(LogLevel, String)>>,
    }

    impl ProgressCallback for Steps {
        fn on_stage(&self, stage: &str, total: u64) {
            self.stages.lock().unwrap().push((stage.into(), total));
        }
        fn on_progress(&self, current: u64, total: u64, message: &str) {
            self.steps
                .lock()
                .unwrap()
                .push((current, total, message.into()));
        }
        fn on_throughput(&self, _: u64) {}
        fn on_log(&self, level: LogLevel, message: &str) {
            self.logs.lock().unwrap().push((level, message.into()));
        }
        fn on_complete(&self, _: bool, _: &str) {}
    }

    /// The pass loop with nothing ever claimed, for the tests of its order,
    /// steps and stops: a scratch maintenance lock, a claim nobody takes, a
    /// reload that is never asked for, nothing filed.
    pub(super) fn drive_pass(
        config: &Config,
        progress: &dyn ProgressCallback,
        stop: &dyn Fn() -> Option<i32>,
        scrub_one: &dyn Fn(&Config, &str, &dyn ProgressCallback, &MaintenanceHeld) -> ScrubFsResult,
    ) -> Vec<ScrubFsResult> {
        let dir = tempfile::tempdir().unwrap();
        let maintenance = dir.path().join("maintenance.lock");
        let mut slot = MaintenanceSlot {
            path: maintenance.clone(),
            poll: Duration::from_millis(10),
            held: Some(
                MaintenanceHeld::try_acquire_at(&maintenance, SCRUB_JOB)
                    .unwrap()
                    .unwrap(),
            ),
        };
        let claim = ClaimWatch::new(&dir.path().join("claim.lock"));
        let adapted = |c: &Config,
                       l: &str,
                       p: &dyn ProgressCallback,
                       h: &MaintenanceHeld,
                       _: &dyn Fn() -> bool| scrub_one(c, l, p, h);
        scrub_targets_yielding(
            config,
            progress,
            &mut slot,
            &claim,
            &[],
            &PassHooks {
                stop,
                reload: &|| panic!("nothing paused, so nothing is read again"),
                scrub_one: &adapted,
                file_result: &|_| {},
                file_yield: &|_| panic!("nothing claimed"),
            },
        )
    }

    /// bd DAS-Backup-Manager-dis3: with no `[scrub].targets` the pass scrubs
    /// every configured `[[target]]`, in config order — never a built-in list
    /// that matches nothing on another install.
    #[test]
    fn a_pass_with_no_scrub_targets_listed_scrubs_every_configured_target() {
        let mut config = Config::default();
        assert!(config.scrub.targets.is_empty());
        for label in ["first", "second"] {
            config.targets.push(crate::config::Target {
                label: label.into(),
                serial: String::new(),
                serials: Vec::new(),
                mount_uuid: None,
                mount: format!("/mnt/backup-{label}"),
                role: crate::config::TargetRole::Primary,
                retention: crate::config::Retention::default(),
                display_name: String::new(),
                esp_partuuid: None,
            });
        }
        let scrubbed = std::sync::Mutex::new(Vec::new());
        let scrub_one = |_: &Config, label: &str, _: &dyn ProgressCallback, _: &MaintenanceHeld| {
            scrubbed.lock().unwrap().push(label.to_string());
            fs_result(label, "uuid", true, 1)
        };
        let results = drive_pass(&config, &NullProgress, &|| None, &scrub_one);
        assert_eq!(*scrubbed.lock().unwrap(), ["first", "second"]);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn a_pass_scrubs_every_target_in_order_with_a_step_and_a_line_each() {
        let mut config = Config::default();
        config.scrub.targets = vec!["primary-22tb".into(), "recovery-a".into()];
        let progress = Steps::default();
        let scrubbed = std::sync::Mutex::new(Vec::new());
        let scrub_one = |_: &Config, label: &str, _: &dyn ProgressCallback, _: &MaintenanceHeld| {
            scrubbed.lock().unwrap().push(label.to_string());
            fs_result(label, "uuid", label == "primary-22tb", 1_000_400)
        };

        let results = drive_pass(&config, &progress, &|| None, &scrub_one);

        assert_eq!(*scrubbed.lock().unwrap(), ["primary-22tb", "recovery-a"]);
        let labels: Vec<&str> = results.iter().map(|r| r.target_label.as_str()).collect();
        assert_eq!(labels, ["primary-22tb", "recovery-a"]);
        assert_eq!(
            *progress.stages.lock().unwrap(),
            [("Scrubbing DAS filesystems".to_string(), 2)]
        );
        assert_eq!(
            *progress.steps.lock().unwrap(),
            [
                (1, 2, "Scrubbing primary-22tb".to_string()),
                (2, 2, "Scrubbing recovery-a".to_string())
            ]
        );
        let failed = &results[1];
        assert_eq!(
            *progress.logs.lock().unwrap(),
            [
                (
                    LogLevel::Info,
                    format!(
                        "primary-22tb: scrub finished cleanly ({} in 100s)",
                        format_bytes(4096)
                    )
                ),
                (
                    LogLevel::Error,
                    format!("recovery-a: scrub FAILED — {}", describe_failure(failed))
                )
            ]
        );
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
            already_scrubbed: Vec::new(),
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

    /// bd DAS-Backup-Manager-su1b.2 (step A3): a target that yielded to a
    /// backup and then finished clean is a clean pass; the report lists each
    /// yield (time, holder, pause) as information, never as a problem.
    #[test]
    fn a_clean_target_that_yielded_reports_clean_and_lists_each_yield() {
        let config = Config::default();
        let mut result = fs_result("primary-22tb", "uuid-a", true, 1_000_400);
        result.yields = vec![
            ScrubYield {
                at_epoch: 1_000_100,
                holder: "backup-run.sh pid 4242".into(),
                paused_secs: Some(1_260),
            },
            ScrubYield {
                at_epoch: 1_000_300,
                holder: "btrdasd backup pid 77".into(),
                paused_secs: None,
            },
        ];
        assert!(result.ok(), "a yield is not an error");
        let pass = pass_with(vec![result]);
        assert!(pass.success());
        let report = format_scrub_report(&pass, &config);
        assert!(report.contains("ALL SCRUBS COMPLETED CLEANLY"), "{report}");
        assert!(!report.contains("PROBLEMS"), "{report}");
        assert!(report.contains("YIELDS TO BACKUPS"), "{report}");
        let at = |e: i64| crate::caldate::local_datetime(e).unwrap()[..16].to_string();
        assert!(
            report.contains(&format!(
                "primary-22tb: yielded at {} to backup-run.sh pid 4242, paused 21m 0s",
                at(1_000_100)
            )),
            "{report}"
        );
        assert!(
            report.contains(&format!(
                "primary-22tb: yielded at {} to btrdasd backup pid 77, pause length unknown",
                at(1_000_300)
            )),
            "{report}"
        );
    }

    /// A pass with no yield prints no yield section: the report of every
    /// pass before A5 is unchanged.
    #[test]
    fn a_pass_without_yields_prints_no_yield_section() {
        let config = Config::default();
        let pass = pass_with(vec![fs_result("primary-22tb", "uuid-a", true, 1_000_400)]);
        assert!(!format_scrub_report(&pass, &config).contains("YIELD"));
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
            already_scrubbed: Vec::new(),
        };
        assert!(pass.success());
        assert!(format_scrub_report(&pass, &config).contains("SKIPPED"));
    }

    #[test]
    fn run_scrub_pass_errors_when_no_targets_configured() {
        let mut config = Config::default();
        config.scrub.targets.clear();
        assert!(config.targets.is_empty());
        let err = run_scrub_pass(&config, &|| Ok(config.clone()), &NullProgress).unwrap_err();
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

    #[test]
    fn a_path_override_is_read_from_the_environment_and_otherwise_the_default_holds() {
        // A variable nothing else sets, so reading its absence cannot race
        // the tests that set the real ones.
        assert_eq!(
            overridable("DAS_B1S_NEVER_SET", "/default/place"),
            PathBuf::from("/default/place")
        );
        temp_env("DAS_B1S_NEVER_SET", "/elsewhere", || {
            assert_eq!(
                overridable("DAS_B1S_NEVER_SET", "/default/place"),
                PathBuf::from("/elsewhere")
            );
        });
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

    /// bd DAS-Backup-Manager-1x76: the pass's report goes to the mailer it is
    /// given — exactly the formatted report, as a Scrub report — and one the
    /// mailer cannot take is a warning.
    #[test]
    fn the_scrub_report_goes_to_the_mailer_it_is_given() {
        let mut config = Config::default();
        config.email.enabled = true;
        config.email.to = "ops@example.org".into();
        config.email.from = "das@example.org".into();
        // Unreachable: never the live relay's port (bd 1x76).
        config.email.smtp_host = "127.0.0.1".into();
        config.email.smtp_port = 1;
        let pass = pass_with(vec![fs_result("primary-22tb", "uuid-a", true, 1_000_400)]);

        let mailer = crate::report::RecordingMailer::accepting();
        let progress = Steps::default();
        email_scrub_report(&pass, &config, &progress, &mailer);
        let sent = mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].body, format_scrub_report(&pass, &config));
        // In the mailer's context (test_mail_context), whatever the
        // environment's DAS_REPORT_TO / DAS_REPORT_FROM say.
        assert_eq!(
            sent[0].subject,
            "[DAS Scrub] nas.example.org — SUCCESS — 2026-11-01 01:02"
        );
        assert_eq!(sent[0].from, "DAS Scrub (nas) <das@example.org>");
        assert_eq!(sent[0].to, "ops@example.org");
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .contains(&(LogLevel::Info, "Scrub report emailed".into()))
        );

        let refusing = crate::report::RecordingMailer::refusing("relay down");
        let progress = Steps::default();
        email_scrub_report(&pass, &config, &progress, &refusing);
        assert_eq!(refusing.sent().len(), 1, "the send is tried");
        assert_eq!(
            *progress.logs.lock().unwrap(),
            [(
                LogLevel::Warning,
                "Could not send scrub report email: relay down".to_string()
            )]
        );

        // Email off: nothing is handed over, and the warning says why.
        config.email.enabled = false;
        let progress = Steps::default();
        email_scrub_report(&pass, &config, &progress, &mailer);
        assert_eq!(mailer.sent().len(), 1);
        assert_eq!(
            *progress.logs.lock().unwrap(),
            [(
                LogLevel::Warning,
                "Could not send scrub report email: Email is not enabled in config".to_string()
            )]
        );
    }
}

/// A pass that is stopped by a signal leaves a record of it (bd
/// DAS-Backup-Manager-weo): `btrdasd scrub status` never shows an older
/// finished pass as the current one.
#[cfg(test)]
mod interrupted_pass_tests {
    use super::tests::drive_pass;
    use super::*;
    use crate::progress::{NullProgress, TestProgress};

    /// Handlers are process-wide; one test at a time touches them.
    static SIGNALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_string()).collect()
    }

    fn running_attempt(pid: Option<u32>, boot_id: Option<String>) -> AttemptState {
        AttemptState {
            outcome: "running".into(),
            started_epoch: 1_000_000,
            pid,
            boot_id,
            ..Default::default()
        }
    }

    fn fs_state(attempt: AttemptState, last_success: Option<i64>) -> FsState {
        FsState {
            target_label: "primary".into(),
            mountpoint: "/mnt/primary".into(),
            last_success_epoch: last_success,
            last_attempt: attempt,
        }
    }

    /// The pass asked to stop while its child ran: the child is stopped and
    /// reaped (the call returns long before the 5 s the child would take), and
    /// the signal is what comes back.
    #[test]
    fn a_signal_stops_the_scrub_child_and_is_reported() {
        let started = Instant::now();
        // Quiet at the pre-spawn check, caught once the child runs.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let outcome = run_scrub_child(
            "sleep",
            &args(&["5"]),
            "",
            Duration::from_millis(20),
            &|| {
                (calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1)
                    .then_some(libc::SIGTERM)
            },
            &|| false,
            &NullProgress,
        );
        assert!(
            matches!(outcome, ScrubRunOutcome::Interrupted(s) if s == libc::SIGTERM),
            "got {outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the child was not stopped: {:?}",
            started.elapsed()
        );
    }

    /// No signal: the child runs to its own end, as before.
    #[test]
    fn without_a_signal_the_child_runs_to_its_end() {
        let outcome = run_scrub_child(
            "true",
            &args(&["x"]),
            "",
            Duration::from_millis(20),
            &|| None,
            &|| false,
            &NullProgress,
        );
        assert!(matches!(outcome, ScrubRunOutcome::Launched(Ok(()))));
    }

    /// A real SIGTERM, delivered to the process while the pass's handlers are
    /// installed, is caught (the process lives) and remembered; dropping the
    /// guard forgets it.
    #[test]
    fn a_real_sigterm_is_caught_and_remembered() {
        let _serial = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
        let guard = catch_termination();
        assert_eq!(pending_signal(), None);
        // SAFETY: raising a signal this process has just installed a handler for.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        assert_eq!(pending_signal(), Some(libc::SIGTERM));
        drop(guard);
        assert_eq!(pending_signal(), None, "the guard clears what it caught");
    }

    #[test]
    fn signals_read_by_name() {
        assert_eq!(signal_name(libc::SIGTERM), "SIGTERM");
        assert_eq!(signal_name(libc::SIGINT), "SIGINT");
        assert_eq!(signal_name(libc::SIGHUP), "SIGHUP");
        assert_eq!(signal_name(libc::SIGUSR1), "signal 10");
    }

    /// The marker written before the scrub starts: `running`, this process,
    /// this boot — and the last success of the earlier pass kept.
    #[test]
    fn the_running_record_is_written_before_the_scrub_and_keeps_the_last_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub-state.json");
        let mut state = ScrubState::default();
        state.filesystems.insert(
            "uuid-a".into(),
            fs_state(
                AttemptState {
                    outcome: "finished".into(),
                    ok: true,
                    ..Default::default()
                },
                Some(900_000),
            ),
        );
        save_state_to(&state, &path).unwrap();

        let marker = RunningMarker {
            label: "primary",
            fsuuid: "uuid-a",
            mountpoint: "/mnt/primary",
            started_epoch: 1_234_567,
        };
        record_running(&path, &marker, &NullProgress).unwrap();

        let fs = load_state_from(&path).unwrap().filesystems["uuid-a"].clone();
        assert_eq!(fs.last_attempt.outcome, "running");
        assert!(!fs.last_attempt.ok);
        assert_eq!(fs.last_attempt.started_epoch, 1_234_567);
        assert_eq!(fs.last_attempt.pid, Some(std::process::id()));
        assert_eq!(fs.last_attempt.boot_id, current_boot_id());
        assert!(fs.last_attempt.boot_id.is_some(), "this host has a boot id");
        assert_eq!(fs.last_success_epoch, Some(900_000), "age still measured");
    }

    /// A record that cannot be written is logged, never silent, and does not
    /// stop the pass.
    #[test]
    fn a_running_record_that_cannot_be_written_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("a-file");
        std::fs::write(&blocker, "x").unwrap();
        // The parent "directory" is a regular file: the write cannot happen.
        let path = blocker.join("scrub-state.json");
        let marker = RunningMarker {
            label: "primary",
            fsuuid: "uuid-a",
            mountpoint: "/mnt/primary",
            started_epoch: 1,
        };
        let progress = TestProgress::default();
        record_running_logged(&path, &marker, &progress);
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter().any(|(level, msg)| *level == LogLevel::Warning
                && msg.contains("running")
                && msg.contains("scrub-state.json")),
            "{logs:?}"
        );
    }

    /// A `running` record is live only for the pass that wrote it: this
    /// process, this boot. Anything else is an interruption nobody recorded.
    #[test]
    fn a_running_record_is_live_only_for_its_own_process_and_boot() {
        let boot = current_boot_id();
        let me = std::process::id();

        let live = running_attempt(Some(me), boot.clone());
        assert!(live.is_live());
        assert_eq!(live.effective_outcome(), "running");

        let other_boot = running_attempt(Some(me), Some("not-this-boot".into()));
        assert!(!other_boot.is_live());
        assert_eq!(other_boot.effective_outcome(), "interrupted");

        let no_pid = running_attempt(None, boot.clone());
        assert_eq!(no_pid.effective_outcome(), "interrupted");

        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = gone.id();
        gone.wait().unwrap();
        let dead = running_attempt(Some(dead_pid), boot);
        assert_eq!(dead.effective_outcome(), "interrupted");

        // Only a running record is reinterpreted.
        let finished = AttemptState {
            outcome: "finished".into(),
            ..Default::default()
        };
        assert_eq!(finished.effective_outcome(), "finished");
    }

    /// bd DAS-Backup-Manager-su1b.2 (step A3): a `yielded` record (the pass
    /// gave the maintenance lock up to a backup and waits to resume) is live
    /// by the same test as `running`: its own process, this boot. A stale
    /// one is a pass that died while paused: `interrupted` (operator Q3).
    #[test]
    fn a_yielded_record_is_live_only_for_its_own_process_and_boot() {
        let boot = current_boot_id();
        let me = std::process::id();
        let yielded = |pid: Option<u32>, boot_id: Option<String>| AttemptState {
            outcome: "yielded".into(),
            yielded_to: Some("backup-run.sh pid 4242".into()),
            yielded_epoch: Some(1_000_100),
            yields: Some(1),
            paused_secs: Some(0),
            ..running_attempt(pid, boot_id)
        };

        let live = yielded(Some(me), boot.clone());
        assert!(live.is_live());
        assert_eq!(live.effective_outcome(), "yielded");
        assert!(!live.ok, "a yielded attempt is not a pass");

        let other_boot = yielded(Some(me), Some("not-this-boot".into()));
        assert!(!other_boot.is_live());
        assert_eq!(other_boot.effective_outcome(), "interrupted");

        let no_pid = yielded(None, boot.clone());
        assert!(!no_pid.is_live());
        assert_eq!(no_pid.effective_outcome(), "interrupted");

        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = gone.id();
        gone.wait().unwrap();
        let dead = yielded(Some(dead_pid), boot);
        assert!(!dead.is_live());
        assert_eq!(dead.effective_outcome(), "interrupted");
    }

    /// The new fields are optional: a record from before them loads, and a
    /// record that does not use them serialises without them, so the state
    /// file stays `schema_version 1`. And a reader that predates `yielded`
    /// (it knows only the weo fields) still loads a `yielded` record and
    /// sees `ok: false`: cautious, not permissive. Rule 10: these hold on
    /// the code as it was, so they cannot be seen red without mutating it.
    #[test]
    fn yield_fields_are_optional_and_an_older_reader_sees_not_ok() {
        let old = format!(
            r#"{{"outcome":"finished","ok":true,"started_epoch":1,"finished_epoch":2,
            "duration_secs":1,"bytes_scrubbed":5,"error_total":0,"counters":{}}}"#,
            serde_json::to_string(&ScrubCounters::default()).unwrap()
        );
        let a: AttemptState = serde_json::from_str(&old).unwrap();
        assert_eq!(a.yielded_to, None);
        assert_eq!(a.yields, None);
        let text = serde_json::to_string(&a).unwrap();
        for key in ["yielded_to", "yielded_epoch", "yields", "paused_secs"] {
            assert!(!text.contains(key), "{key} written when unused: {text}");
        }

        #[derive(Deserialize)]
        struct OlderReader {
            outcome: String,
            ok: bool,
        }
        let yielded = AttemptState {
            outcome: "yielded".into(),
            pid: Some(1),
            boot_id: Some("b".into()),
            yielded_to: Some("backup-run.sh pid 4242".into()),
            yielded_epoch: Some(10),
            yields: Some(2),
            paused_secs: Some(30),
            ..Default::default()
        };
        let text = serde_json::to_string(&yielded).unwrap();
        let older: OlderReader = serde_json::from_str(&text).unwrap();
        assert_eq!(older.outcome, "yielded");
        assert!(!older.ok);
        let back: AttemptState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.yielded_to.as_deref(), Some("backup-run.sh pid 4242"));
        assert_eq!(
            (back.yielded_epoch, back.yields, back.paused_secs),
            (Some(10), Some(2), Some(30))
        );
    }

    /// The pass's own result for a caught signal: `interrupted`, with the
    /// signal's name, not ok — and the earlier success still the age anchor.
    #[test]
    fn an_interrupted_result_is_recorded_as_interrupted_with_its_signal() {
        let mut result = ScrubFsResult::new("primary");
        result.fsuuid = "uuid-a".into();
        result.mountpoint = "/mnt/primary".into();
        result.started_epoch = 1_000_000;
        result.finished_epoch = 1_003_000;
        result.interrupted_by = Some(libc::SIGTERM);
        result
            .errors
            .push("scrub interrupted by SIGTERM".to_string());
        let pass = ScrubPass {
            status: PassStatus::Completed,
            started_epoch: 1_000_000,
            finished_epoch: 1_003_000,
            results: vec![result],
            errors: Vec::new(),
            already_scrubbed: Vec::new(),
        };
        let mut state = ScrubState::default();
        state.filesystems.insert(
            "uuid-a".into(),
            fs_state(
                AttemptState {
                    outcome: "finished".into(),
                    ok: true,
                    ..Default::default()
                },
                Some(900_000),
            ),
        );
        merge_pass_into_state(&mut state, &pass);
        let fs = &state.filesystems["uuid-a"];
        assert_eq!(fs.last_attempt.outcome, "interrupted");
        assert_eq!(fs.last_attempt.signal.as_deref(), Some("SIGTERM"));
        assert!(!fs.last_attempt.ok);
        assert_eq!(fs.last_success_epoch, Some(900_000));
        assert_eq!(pass.interrupted_by(), Some(libc::SIGTERM));
        assert_eq!(pass.results[0].status_word(), "INTERRUPTED");
    }

    // --- bd DAS-Backup-Manager-su1b.22: an interrupted attempt's figures ---

    /// The 2026-11-23 record shape: a RAID-1 pair, one device with damage that
    /// scrub corrected, the pass stopped by SIGTERM (`canceled:1`).
    fn damaged_record(t_start: i64) -> ScrubStatusRecord {
        parse_scrub_status(
            &format!(
                "scrub status:1\n5eed0016-0000-4000-8000-000000000016:1|data_extents_scrubbed:1|\
                 tree_extents_scrubbed:1|data_bytes_scrubbed:4096|tree_bytes_scrubbed:0|\
                 read_errors:0|csum_errors:0|verify_errors:0|no_csum:0|csum_discards:0|\
                 super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:0|\
                 last_physical:1|t_start:{t_start}|t_resumed:0|duration:5|canceled:1|finished:0\n\
                 5eed0016-0000-4000-8000-000000000016:2|data_extents_scrubbed:1|\
                 tree_extents_scrubbed:1|data_bytes_scrubbed:6960000000000|tree_bytes_scrubbed:0|\
                 read_errors:0|csum_errors:437204|verify_errors:31068|no_csum:0|csum_discards:0|\
                 super_errors:0|malloc_errors:0|uncorrectable_errors:0|corrected_errors:468272|\
                 last_physical:1|t_start:{t_start}|t_resumed:0|duration:5|canceled:1|finished:0\n"
            ),
            "5eed0016-0000-4000-8000-000000000016",
        )
        .unwrap()
    }

    fn interrupted_result() -> ScrubFsResult {
        let mut r = ScrubFsResult::new("primary");
        r.fsuuid = "5eed0016-0000-4000-8000-000000000016".into();
        r.started_epoch = 1_000_000;
        r.finished_epoch = 1_003_000;
        r.interrupted_by = Some(libc::SIGTERM);
        r.errors.push("scrub interrupted by SIGTERM".to_string());
        r
    }

    fn filed(result: &ScrubFsResult) -> AttemptState {
        let mut state = ScrubState::default();
        merge_result_into_state(&mut state, result, 1_003_000);
        state.filesystems[&result.fsuuid].last_attempt.clone()
    }

    /// The observed defect: the record held the damage, the state file said 0.
    #[test]
    fn an_interrupted_attempt_files_the_counters_of_its_status_record() {
        let mut r = interrupted_result();
        take_interrupted_figures(&mut r, Ok(damaged_record(1_000_000)), 1_000_000);
        let a = filed(&r);
        assert_eq!(a.outcome, "interrupted");
        assert!(!a.counters_unknown);
        assert_eq!(a.counters.csum_errors, 437_204);
        assert_eq!(a.counters.verify_errors, 31_068);
        assert_eq!(a.counters.corrected_errors, 468_272);
        assert_eq!(a.error_total, 437_204 + 31_068 + 468_272);
        assert_eq!(a.bytes_scrubbed, 6_960_000_004_096);
        assert!(a.counters_summary().contains("corrected=468272"));
    }

    /// No record, or one from an earlier run: the figures are unknown, and
    /// every reader says so instead of "no errors".
    #[test]
    fn an_interrupted_attempt_without_a_usable_record_files_unknown() {
        let missing = Err(ScrubError::StatusMissing {
            path: "/nonexistent".into(),
        });
        let mut r = interrupted_result();
        take_interrupted_figures(&mut r, missing, 1_000_000);
        assert!(r.counters_unknown());
        assert_eq!(r.counters_summary(), "unknown");
        let a = filed(&r);
        assert!(a.counters_unknown);
        assert_eq!(a.counters_summary(), "unknown");
        assert_eq!(a.error_total_known(), None);

        // A record from a scrub that ended long before this one began.
        let mut stale = interrupted_result();
        take_interrupted_figures(&mut stale, Ok(damaged_record(500_000)), 1_000_000);
        assert!(stale.counters_unknown(), "a stale record is not this run's");
        assert_eq!(stale.counters, ScrubCounters::default());
    }

    /// A paused filesystem has measured nothing yet, and is still no failure.
    #[test]
    fn a_paused_attempt_files_unknown_figures_and_stays_not_a_failure() {
        let mut r = ScrubFsResult::new("primary");
        r.fsuuid = "5eed0016-0000-4000-8000-000000000016".into();
        r.paused = Some(PausedBy {
            by: "btrdasd scrub pause".into(),
            at_epoch: 1_002_000,
        });
        assert!(r.counters_unknown());
        let a = filed(&r);
        assert_eq!(a.outcome, "paused");
        assert!(a.counters_unknown);
        assert_eq!(a.error_total_known(), None);
    }

    /// A finished pass is exactly as before: its zeros are measurements.
    #[test]
    fn a_finished_attempt_stays_measured() {
        let mut r = ScrubFsResult::new("primary");
        r.fsuuid = "5eed0016-0000-4000-8000-000000000016".into();
        r.outcome = Some(ScrubOutcome::Finished);
        assert!(!r.counters_unknown());
        let a = filed(&r);
        assert!(!a.counters_unknown);
        assert_eq!(a.counters_summary(), "no errors");
        assert_eq!(a.error_total_known(), Some(0));
    }

    /// A state file written before this field existed still loads, its
    /// figures read as measured (as they always did).
    #[test]
    fn an_old_format_state_file_still_loads() {
        let old = r#"{"schema_version":1,"filesystems":{"u":{"target_label":"t",
            "mountpoint":"/mnt/t","last_success_epoch":5,"last_attempt":{
            "outcome":"interrupted","ok":false,"started_epoch":1,"finished_epoch":2,
            "duration_secs":1,"bytes_scrubbed":0,"error_total":0,"counters":{"read_errors":0,"csum_errors":0,"verify_errors":0,
            "super_errors":0,"malloc_errors":0,"uncorrectable_errors":0,
            "corrected_errors":0,"no_csum":0,"csum_discards":0}}}}}"#;
        let state: ScrubState = serde_json::from_str(old).unwrap();
        let a = &state.filesystems["u"].last_attempt;
        assert!(!a.counters_unknown);
        assert_eq!(a.error_total_known(), Some(0));
        // A measured attempt serialises without the new key.
        let json = serde_json::to_string(a).unwrap();
        assert!(!json.contains("counters_unknown"), "{json}");
    }

    /// The email/report prints `unknown` for an interrupted filesystem whose
    /// record could not be read, never `errors=no errors` or `0 B`.
    #[test]
    fn the_report_says_unknown_for_unread_figures() {
        let mut r = interrupted_result();
        take_interrupted_figures(&mut r, Err(ScrubError::StatusMissingHeader), 1_000_000);
        let pass = ScrubPass {
            status: PassStatus::Completed,
            started_epoch: 1_000_000,
            finished_epoch: 1_003_000,
            results: vec![r],
            errors: Vec::new(),
            already_scrubbed: Vec::new(),
        };
        let text = format_scrub_report(&pass, &Config::default());
        assert!(text.contains("errors=unknown"), "{text}");
        assert!(!text.contains("errors=no errors"), "{text}");
    }

    /// A result is filed the moment its filesystem is done, so a pass killed
    /// during the next target leaves this one recorded, not "running".
    #[test]
    fn a_finished_target_is_recorded_before_the_next_one_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub-state.json");
        let marker = RunningMarker {
            label: "primary",
            fsuuid: "uuid-a",
            mountpoint: "/mnt/primary",
            started_epoch: 1,
        };
        record_running(&path, &marker, &NullProgress).unwrap();
        let mut result = ScrubFsResult::new("primary");
        result.fsuuid = "uuid-a".into();
        result.outcome = Some(ScrubOutcome::Finished);
        result.started_epoch = 1;
        result.finished_epoch = 500;
        record_result_logged(&path, &result, &NullProgress);
        let fs = load_state_from(&path).unwrap().filesystems["uuid-a"].clone();
        assert_eq!(fs.last_attempt.outcome, "finished");
        assert_eq!(fs.last_success_epoch, Some(500));
        assert_eq!(fs.last_attempt.pid, None);
    }

    fn two_target_config() -> Config {
        let mut config = Config::default();
        config.scrub.targets = vec!["first".into(), "second".into()];
        config
    }

    /// A scrub stopped by a signal ends the pass: the next target is not begun.
    #[test]
    fn an_interrupted_target_ends_the_pass() {
        let ran = std::sync::Mutex::new(Vec::new());
        let scrub_one = |_: &Config, label: &str, _: &dyn ProgressCallback, _: &MaintenanceHeld| {
            ran.lock().unwrap().push(label.to_string());
            let mut r = ScrubFsResult::new(label);
            r.interrupted_by = Some(libc::SIGTERM);
            r
        };
        let results = drive_pass(&two_target_config(), &NullProgress, &|| None, &scrub_one);
        assert_eq!(*ran.lock().unwrap(), ["first"]);
        assert_eq!(results.len(), 1);
    }

    /// A signal that arrives between targets: the target it kept from starting
    /// is listed as interrupted (so the pass is), and nothing after it starts.
    #[test]
    fn a_signal_between_targets_marks_the_unstarted_target_interrupted() {
        let ran = std::sync::Mutex::new(Vec::new());
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let stop = || {
            // Quiet for the first target, caught before the second.
            (calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1).then_some(libc::SIGHUP)
        };
        let scrub_one = |_: &Config, label: &str, _: &dyn ProgressCallback, _: &MaintenanceHeld| {
            ran.lock().unwrap().push(label.to_string());
            let mut r = ScrubFsResult::new(label);
            r.outcome = Some(ScrubOutcome::Finished);
            r
        };
        let results = drive_pass(&two_target_config(), &NullProgress, &stop, &scrub_one);
        assert_eq!(*ran.lock().unwrap(), ["first"]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].target_label, "second");
        assert_eq!(results[1].interrupted_by, Some(libc::SIGHUP));
        assert!(!results[1].scrub_launched);
        assert!(!results[1].ok());
    }
}

/// The yield (bd DAS-Backup-Manager-su1b.2, step A5): a backup's claim makes
/// the pass stop its scrub, unmount, give the maintenance lock up, wait, and
/// resume the same filesystem. Real `flock`s on scratch files throughout; the
/// claimant is a thread holding a real [`crate::maintenance::Claim`].
#[cfg(test)]
mod yield_tests {
    use super::*;
    use crate::maintenance::{Claim, claim_is_held};
    use crate::progress::{NullProgress, TestProgress};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Arc, Mutex, mpsc};

    const POLL: Duration = Duration::from_millis(10);

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_string()).collect()
    }

    /// A record of filesystem `u` as btrfs-progs writes it.
    fn record(t_start: i64, t_resumed: i64, canceled: u8, finished: u8) -> ScrubStatusRecord {
        parse_scrub_status(
            &format!(
                "scrub status:1\nu:1|data_extents_scrubbed:1|tree_extents_scrubbed:1|\
                 data_bytes_scrubbed:4096|tree_bytes_scrubbed:4096|read_errors:0|csum_errors:0|\
                 verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|\
                 uncorrectable_errors:0|corrected_errors:0|last_physical:8192|t_start:{t_start}|\
                 t_resumed:{t_resumed}|duration:10|canceled:{canceled}|finished:{finished}\n"
            ),
            "u",
        )
        .unwrap()
    }

    /// Two scrub targets the loop can resolve by UUID without a drive.
    fn config() -> Config {
        let mut config = Config::default();
        for label in ["first", "second"] {
            config.targets.push(crate::config::Target {
                label: label.into(),
                serial: String::new(),
                serials: Vec::new(),
                mount_uuid: Some(format!("u-{label}")),
                mount: format!("/mnt/{label}"),
                role: crate::config::TargetRole::Primary,
                retention: crate::config::Retention::default(),
                display_name: String::new(),
                esp_partuuid: None,
            });
        }
        config.scrub.targets = vec!["first".into(), "second".into()];
        config
    }

    fn clean(label: &str) -> ScrubFsResult {
        let mut r = ScrubFsResult::new(label);
        r.fsuuid = format!("u-{label}");
        r.outcome = Some(ScrubOutcome::Finished);
        r.scrub_launched = true;
        r.started_epoch = 1_000;
        r.finished_epoch = 2_000;
        r
    }

    /// A segment that stopped for the claim, its unmount attempted.
    fn paused(label: &str, resume: bool) -> ScrubFsResult {
        let mut r = ScrubFsResult::new(label);
        r.fsuuid = format!("u-{label}");
        r.scrub_launched = true;
        r.started_epoch = 1_000;
        r.pending_yield = Some(PendingYield { resume });
        r
    }

    struct Scratch {
        _dir: tempfile::TempDir,
        maintenance: PathBuf,
        claim: PathBuf,
    }

    fn scratch() -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        Scratch {
            maintenance: dir.path().join("maintenance.lock"),
            claim: dir.path().join("claim.lock"),
            _dir: dir,
        }
    }

    fn slot_held(path: &Path) -> MaintenanceSlot {
        MaintenanceSlot {
            path: path.to_path_buf(),
            poll: POLL,
            held: Some(
                MaintenanceHeld::try_acquire_at(path, "btrdasd scrub run")
                    .unwrap()
                    .unwrap(),
            ),
        }
    }

    /// Whether this process could take the lock at `path` now (and lets go).
    fn lock_is_free(path: &Path) -> bool {
        FileLock::try_acquire(path).unwrap().is_some()
    }

    fn wait_until(limit: Duration, what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + limit;
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A backup in a thread, as `acquire_manual_locks_at` will be: on `go`,
    /// claim; wait for the maintenance lock; once it has it, let the claim
    /// go; hold the lock for `hold`, then let it go. `ran` is set while it
    /// holds the lock.
    fn backup(
        s: &Scratch,
        go: mpsc::Receiver<()>,
        ran: Arc<AtomicBool>,
        hold: Duration,
    ) -> std::thread::JoinHandle<()> {
        let (maintenance, claim) = (s.maintenance.clone(), s.claim.clone());
        std::thread::spawn(move || {
            if go.recv().is_err() {
                return;
            }
            let claimed = Claim::take_at(&claim, "test backup").unwrap();
            let held = loop {
                if let Some(h) =
                    MaintenanceHeld::try_acquire_at(&maintenance, "test backup").unwrap()
                {
                    break h;
                }
                std::thread::sleep(Duration::from_millis(2));
            };
            drop(claimed);
            ran.store(true, Ordering::SeqCst);
            std::thread::sleep(hold);
            drop(held);
        })
    }

    struct Filed {
        results: Mutex<Vec<String>>,
        yields: Mutex<Vec<YieldMark>>,
    }

    fn filed() -> Filed {
        Filed {
            results: Mutex::new(Vec::new()),
            yields: Mutex::new(Vec::new()),
        }
    }

    // --- the claim probe ---------------------------------------------------

    #[test]
    fn the_claim_is_seen_while_a_backup_holds_it_and_not_once_it_lets_go() {
        let s = scratch();
        let watch = ClaimWatch::new(&s.claim);
        assert!(!watch.wants_lock(&NullProgress));
        assert!(!watch.still_waiting(&NullProgress));
        let claimed = Claim::take_at(&s.claim, "test backup").unwrap();
        assert!(watch.wants_lock(&NullProgress));
        assert!(watch.still_waiting(&NullProgress));
        assert_eq!(
            watch.holder(),
            format!("test backup pid {}", std::process::id())
        );
        drop(claimed);
        assert!(!watch.wants_lock(&NullProgress));
    }

    /// A probe that cannot look reads as a claim once (the pass yields), then
    /// as none until it can look again: one pause, never a pass that stops
    /// every poll or waits outside for ever. Said, as a warning.
    #[test]
    fn a_probe_that_cannot_look_costs_one_pause_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "").unwrap();
        let watch = ClaimWatch::new(&dir.path().join("file").join("claim.lock"));
        let progress = TestProgress::default();
        assert!(watch.wants_lock(&progress), "the first failure yields");
        assert!(!watch.still_waiting(&progress), "outside, it does not wait");
        assert!(!watch.wants_lock(&progress), "and it does not yield again");
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter().any(
                |(l, m)| *l == LogLevel::Warning && m.contains("cannot look at the claim lock")
            ),
            "{logs:?}"
        );
    }

    // --- the stop ----------------------------------------------------------

    #[test]
    fn a_claim_before_the_spawn_starts_no_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let outcome = run_scrub_child(
            "touch",
            &args(&[marker.to_str().unwrap()]),
            "",
            POLL,
            &|| None,
            &|| true,
            &NullProgress,
        );
        assert!(
            matches!(outcome, ScrubRunOutcome::Yielded { launched: false }),
            "{outcome:?}"
        );
        assert!(!marker.exists(), "no child may start for a claimed lock");
    }

    /// A claim while the child runs stops it (and reaps it) long before the
    /// 5 s it would take, and says it yielded, not that it was interrupted.
    #[test]
    fn a_claim_while_the_child_runs_stops_and_reaps_it() {
        let started = Instant::now();
        let calls = AtomicUsize::new(0);
        let outcome = run_scrub_child(
            "sleep",
            &args(&["5"]),
            "",
            Duration::from_millis(20),
            &|| None,
            &|| calls.fetch_add(1, Ordering::SeqCst) >= 1,
            &NullProgress,
        );
        assert!(
            matches!(outcome, ScrubRunOutcome::Yielded { launched: true }),
            "{outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_signal_outranks_a_claim() {
        let outcome = run_scrub_child(
            "sleep",
            &args(&["5"]),
            "",
            POLL,
            &|| Some(libc::SIGTERM),
            &|| true,
            &NullProgress,
        );
        assert!(
            matches!(outcome, ScrubRunOutcome::Interrupted(s) if s == libc::SIGTERM),
            "{outcome:?}"
        );
    }

    // --- what the stopped segment allows -----------------------------------

    #[test]
    fn a_segment_that_never_started_pauses_and_resumes() {
        assert_eq!(
            yield_verdict(false, LiveScrubState::Unknown, None, 1_000),
            YieldVerdict::Pause { note: None }
        );
    }

    #[test]
    fn a_kernel_not_seen_to_stop_is_never_unmounted() {
        for live in [LiveScrubState::Running, LiveScrubState::Unknown] {
            let fresh = record(1_000, 0, 0, 0);
            assert!(
                matches!(
                    yield_verdict(true, live, Some(&fresh), 1_000),
                    YieldVerdict::KernelLive(_)
                ),
                "{live:?}"
            );
        }
    }

    #[test]
    fn a_fresh_unfinished_record_pauses_and_resumes() {
        let fresh = record(1_000, 0, 0, 0);
        assert_eq!(
            yield_verdict(true, LiveScrubState::NotRunning, Some(&fresh), 1_000),
            YieldVerdict::Pause { note: None }
        );
        // A resumed segment's record is fresh by its resume time.
        let resumed = record(10, 1_000, 0, 0);
        assert_eq!(
            yield_verdict(true, LiveScrubState::NotRunning, Some(&resumed), 1_000),
            YieldVerdict::Pause { note: None }
        );
    }

    #[test]
    fn a_filesystem_that_finished_as_it_was_stopped_is_a_result() {
        let fresh = record(1_000, 0, 0, 1);
        assert_eq!(
            yield_verdict(true, LiveScrubState::NotRunning, Some(&fresh), 1_000),
            YieldVerdict::Finished
        );
    }

    /// Last month's finished record, seen before this segment wrote its own,
    /// is never this segment's result: the pass pauses, and the resume starts
    /// the filesystem afresh.
    #[test]
    fn a_stale_finished_record_is_not_a_result() {
        let stale = record(10, 0, 0, 1);
        assert!(matches!(
            yield_verdict(true, LiveScrubState::NotRunning, Some(&stale), 1_000_000),
            YieldVerdict::Pause { note: Some(_) }
        ));
        assert!(matches!(
            yield_verdict(true, LiveScrubState::NotRunning, None, 1_000_000),
            YieldVerdict::Pause { note: Some(_) }
        ));
    }

    #[test]
    fn a_canceled_record_pauses_and_says_the_resume_starts_from_zero() {
        let canceled = record(1_000, 0, 1, 1);
        let YieldVerdict::Pause { note: Some(note) } =
            yield_verdict(true, LiveScrubState::NotRunning, Some(&canceled), 1_000)
        else {
            panic!("a canceled record pauses with a note");
        };
        assert!(note.contains("from zero"), "{note}");
    }

    // --- waiting for the lock behind a claim -------------------------------

    /// Checkpoint 1: while a backup claims, the pass does not take the lock,
    /// even though it is free; once the claim goes, it does.
    #[test]
    fn the_pass_waits_while_claimed_without_taking_the_free_lock() {
        let s = scratch();
        let claimed = Claim::take_at(&s.claim, "test backup").unwrap();
        let (maintenance, claim) = (s.maintenance.clone(), s.claim.clone());
        let waiter = std::thread::spawn(move || {
            let watch = ClaimWatch::new(&claim);
            take_maintenance_behind_claims(
                &maintenance,
                POLL,
                &|| watch.still_waiting(&NullProgress),
                &|| watch.wants_lock(&NullProgress),
                &|| None,
                &|| None,
                &NullProgress,
            )
            .map(|r| matches!(r, Retake::Held(_)))
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiter.is_finished(), "the pass went ahead of a claim");
        assert!(
            lock_is_free(&s.maintenance),
            "the pass took the lock while claimed"
        );
        drop(claimed);
        assert!(waiter.join().unwrap().unwrap(), "taken once the claim went");
    }

    /// Checkpoint 2: a claim made while the pass took the lock: it is let go
    /// unused, and taken again only once nobody claims.
    #[test]
    fn a_lock_claimed_as_it_was_taken_is_let_go_unused() {
        let s = scratch();
        let wants = AtomicUsize::new(0);
        let outcome = take_maintenance_behind_claims(
            &s.maintenance,
            POLL,
            &|| false,
            &|| wants.fetch_add(1, Ordering::SeqCst) == 0,
            &|| None,
            &|| None,
            &NullProgress,
        )
        .unwrap();
        assert!(matches!(outcome, Retake::Held(_)));
        assert_eq!(
            wants.load(Ordering::SeqCst),
            2,
            "taken twice, kept the second time"
        );
    }

    /// A signal ends the wait at once, holding nothing — a blocking flock
    /// would retry through it.
    #[test]
    fn a_signal_ends_the_wait_holding_nothing() {
        let s = scratch();
        let other = FileLock::try_acquire(&s.maintenance).unwrap().unwrap();
        let calls = AtomicUsize::new(0);
        let started = Instant::now();
        let outcome = take_maintenance_behind_claims(
            &s.maintenance,
            POLL,
            &|| false,
            &|| false,
            &|| (calls.fetch_add(1, Ordering::SeqCst) >= 3).then_some(libc::SIGTERM),
            &|| None,
            &NullProgress,
        )
        .unwrap();
        assert!(matches!(outcome, Retake::Interrupted(s) if s == libc::SIGTERM));
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(other);
        assert!(lock_is_free(&s.maintenance));
    }

    // --- the pass loop -----------------------------------------------------

    /// Checkpoint 4 and the yield: a claim during a target. The hold is the
    /// pass's while the segment stops and unmounts (the backup cannot get it);
    /// only then is it given up; the backup runs; the SAME target is resumed,
    /// then the next. One `yielded` record, no result filed for the segment,
    /// and the finished target lists its pause.
    #[test]
    fn a_claim_during_a_target_yields_then_resumes_the_same_target() {
        let s = scratch();
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim);
        let (go, rx) = mpsc::channel();
        let ran = Arc::new(AtomicBool::new(false));
        let backup = backup(&s, rx, ran.clone(), Duration::from_millis(30));
        let calls = Mutex::new(Vec::new());
        let maintenance = s.maintenance.clone();
        let scrub_one = |_: &Config,
                         label: &str,
                         _: &dyn ProgressCallback,
                         _: &MaintenanceHeld,
                         claimed: &dyn Fn() -> bool| {
            assert!(!lock_is_free(&maintenance), "scrubbing without the lock");
            let first = calls.lock().unwrap().is_empty();
            calls
                .lock()
                .unwrap()
                .push((label.to_string(), ran.load(Ordering::SeqCst)));
            if first {
                go.send(()).unwrap();
                wait_until(Duration::from_secs(5), "the claim", claimed);
                // Stopping and unmounting: the backup cannot have the lock.
                std::thread::sleep(Duration::from_millis(50));
                assert!(
                    !ran.load(Ordering::SeqCst),
                    "the lock went before the unmount"
                );
                return paused(label, true);
            }
            clean(label)
        };
        let f = filed();
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &[],
            &PassHooks {
                stop: &|| None,
                reload: &|| Ok(config()),
                scrub_one: &scrub_one,
                file_result: &|r| f.results.lock().unwrap().push(r.target_label.clone()),
                file_yield: &|m| f.yields.lock().unwrap().push(m.clone()),
            },
        );
        backup.join().unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                ("first".to_string(), false),
                ("first".to_string(), true),
                ("second".to_string(), true)
            ]
        );
        assert_eq!(results.len(), 2);
        assert!(results[0].ok(), "{:?}", results[0]);
        assert!(results[0].pending_yield.is_none());
        assert_eq!(results[0].yields.len(), 1);
        assert_eq!(
            results[0].yields[0].holder,
            format!("test backup pid {}", std::process::id())
        );
        assert!(results[0].yields[0].paused_secs.is_some());
        assert!(results[1].yields.is_empty());
        assert_eq!(*f.results.lock().unwrap(), ["first", "second"]);
        let yields = f.yields.lock().unwrap();
        assert_eq!(yields.len(), 1);
        assert_eq!(yields[0].label, "first");
        assert_eq!(yields[0].fsuuid, "u-first");
        assert_eq!(yields[0].mountpoint, "/mnt/first");
        assert_eq!(yields[0].yields, 1);
        assert_eq!(yields[0].started_epoch, 1_000);
        assert!(
            slot.held().is_some(),
            "the pass holds the lock again at its end"
        );
    }

    /// Checkpoint 3: a claim between targets yields with nothing mounted —
    /// the next target is not begun until the backup is done — and its
    /// `yielded` record names it, so a pass that dies there is not silent.
    #[test]
    fn a_claim_between_targets_yields_before_the_next_mount() {
        let s = scratch();
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim);
        let (go, rx) = mpsc::channel();
        let ran = Arc::new(AtomicBool::new(false));
        let backup = backup(&s, rx, ran.clone(), Duration::from_millis(30));
        let calls = Mutex::new(Vec::new());
        let claim = s.claim.clone();
        let scrub_one = |_: &Config,
                         label: &str,
                         _: &dyn ProgressCallback,
                         _: &MaintenanceHeld,
                         _: &dyn Fn() -> bool| {
            calls
                .lock()
                .unwrap()
                .push((label.to_string(), ran.load(Ordering::SeqCst)));
            if label == "first" {
                go.send(()).unwrap();
                wait_until(Duration::from_secs(5), "the claim", || {
                    claim_is_held(&claim).unwrap()
                });
            }
            clean(label)
        };
        let f = filed();
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &[],
            &PassHooks {
                stop: &|| None,
                reload: &|| Ok(config()),
                scrub_one: &scrub_one,
                file_result: &|r| f.results.lock().unwrap().push(r.target_label.clone()),
                file_yield: &|m| f.yields.lock().unwrap().push(m.clone()),
            },
        );
        backup.join().unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [("first".to_string(), false), ("second".to_string(), true)]
        );
        assert!(results.iter().all(ScrubFsResult::ok));
        assert_eq!(results[1].yields.len(), 1);
        let yields = f.yields.lock().unwrap();
        assert_eq!(yields.len(), 1);
        assert_eq!(yields[0].label, "second");
        assert_eq!(yields[0].fsuuid, "u-second");
    }

    /// A signal while the pass waits for a backup: the target is filed
    /// `interrupted` with its pause, nothing more is scrubbed, and the pass
    /// holds no maintenance lock.
    #[test]
    fn a_signal_while_yielded_files_interrupted_and_holds_nothing() {
        let s = scratch();
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim);
        let claimant = Arc::new(Mutex::new(None));
        let calls = AtomicUsize::new(0);
        let claim = s.claim.clone();
        let held_claim = claimant.clone();
        let scrub_one = move |_: &Config,
                              label: &str,
                              _: &dyn ProgressCallback,
                              _: &MaintenanceHeld,
                              _: &dyn Fn() -> bool| {
            calls.fetch_add(1, Ordering::SeqCst);
            // A claimant that never takes the lock: the pass waits outside.
            *held_claim.lock().unwrap() = Some(Claim::take_at(&claim, "test backup").unwrap());
            paused(label, true)
        };
        let f = filed();
        let stop = || (!f.yields.lock().unwrap().is_empty()).then_some(libc::SIGTERM);
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &[],
            &PassHooks {
                stop: &stop,
                reload: &|| Ok(config()),
                scrub_one: &scrub_one,
                file_result: &|r| f.results.lock().unwrap().push(r.target_label.clone()),
                file_yield: &|m| f.yields.lock().unwrap().push(m.clone()),
            },
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].interrupted_by, Some(libc::SIGTERM));
        assert_eq!(results[0].yields.len(), 1);
        assert!(!results[0].ok());
        assert!(results[0].scrub_launched);
        assert_eq!(*f.results.lock().unwrap(), ["first"]);
        assert!(
            slot.held().is_none(),
            "a pass stopped while yielded holds no lock"
        );
        assert!(lock_is_free(&s.maintenance));
        drop(claimant);
    }

    /// After a pause the configuration is read again before anything is
    /// mounted: the resumed target is scrubbed by the reloaded config.
    #[test]
    fn a_resumed_target_runs_on_the_configuration_read_after_the_pause() {
        let s = scratch();
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim);
        let (go, rx) = mpsc::channel();
        let ran = Arc::new(AtomicBool::new(false));
        let backup = backup(&s, rx, ran.clone(), Duration::from_millis(10));
        let seen = Mutex::new(Vec::new());
        let scrub_one = |cfg: &Config,
                         label: &str,
                         _: &dyn ProgressCallback,
                         _: &MaintenanceHeld,
                         claimed: &dyn Fn() -> bool| {
            seen.lock().unwrap().push(cfg.das.mount_opts.clone());
            if seen.lock().unwrap().len() == 1 {
                go.send(()).unwrap();
                wait_until(Duration::from_secs(5), "the claim", claimed);
                return paused(label, true);
            }
            clean(label)
        };
        let reloads = AtomicUsize::new(0);
        let reload = || {
            reloads.fetch_add(1, Ordering::SeqCst);
            let mut c = config();
            c.das.mount_opts = "reloaded".into();
            Ok(c)
        };
        let f = filed();
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &[],
            &PassHooks {
                stop: &|| None,
                reload: &reload,
                scrub_one: &scrub_one,
                file_result: &|_| {},
                file_yield: &|m| f.yields.lock().unwrap().push(m.clone()),
            },
        );
        backup.join().unwrap();
        assert_eq!(results.len(), 2);
        let first_opts = Config::default().das.mount_opts;
        assert_eq!(
            *seen.lock().unwrap(),
            [first_opts.as_str(), "reloaded", "reloaded"]
        );
        assert_eq!(
            reloads.load(Ordering::SeqCst),
            1,
            "read again after the pause only"
        );
    }

    /// A configuration that cannot be read after the pause: nothing more is
    /// mounted, and the paused target fails visibly, naming why.
    #[test]
    fn an_unreadable_configuration_after_the_pause_ends_the_pass_visibly() {
        let s = scratch();
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim);
        let (go, rx) = mpsc::channel();
        let ran = Arc::new(AtomicBool::new(false));
        let backup = backup(&s, rx, ran.clone(), Duration::from_millis(10));
        let calls = AtomicUsize::new(0);
        let scrub_one = |_: &Config,
                         label: &str,
                         _: &dyn ProgressCallback,
                         _: &MaintenanceHeld,
                         claimed: &dyn Fn() -> bool| {
            calls.fetch_add(1, Ordering::SeqCst);
            go.send(()).unwrap();
            wait_until(Duration::from_secs(5), "the claim", claimed);
            paused(label, true)
        };
        let f = filed();
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &[],
            &PassHooks {
                stop: &|| None,
                reload: &|| Err("boom: config.toml unreadable".to_string()),
                scrub_one: &scrub_one,
                file_result: &|r| f.results.lock().unwrap().push(r.target_label.clone()),
                file_yield: &|_| {},
            },
        );
        backup.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(results.len(), 1);
        assert!(!results[0].ok());
        assert!(
            results[0].errors.iter().any(|e| e.contains("boom")),
            "{:?}",
            results[0].errors
        );
        assert_eq!(*f.results.lock().unwrap(), ["first"]);
    }

    /// A target the reloaded configuration no longer scrubs is not resumed,
    /// and says so; the rest of the reloaded list goes on.
    #[test]
    fn a_target_gone_from_the_reloaded_configuration_is_not_resumed() {
        let s = scratch();
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim);
        let (go, rx) = mpsc::channel();
        let ran = Arc::new(AtomicBool::new(false));
        let backup = backup(&s, rx, ran.clone(), Duration::from_millis(10));
        let calls = Mutex::new(Vec::new());
        let scrub_one = |_: &Config,
                         label: &str,
                         _: &dyn ProgressCallback,
                         _: &MaintenanceHeld,
                         claimed: &dyn Fn() -> bool| {
            calls.lock().unwrap().push(label.to_string());
            if label == "first" {
                go.send(()).unwrap();
                wait_until(Duration::from_secs(5), "the claim", claimed);
                return paused(label, true);
            }
            clean(label)
        };
        let reload = || {
            let mut c = config();
            c.scrub.targets = vec!["second".into()];
            Ok(c)
        };
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &[],
            &PassHooks {
                stop: &|| None,
                reload: &reload,
                scrub_one: &scrub_one,
                file_result: &|_| {},
                file_yield: &|_| {},
            },
        );
        backup.join().unwrap();
        assert_eq!(*calls.lock().unwrap(), ["first", "second"]);
        assert_eq!(results.len(), 2);
        assert!(!results[0].ok());
        assert!(
            results[0].errors.iter().any(|e| e.contains("no longer")),
            "{:?}",
            results[0].errors
        );
        assert!(results[1].ok());
    }

    /// A segment whose kernel scrub was not seen to stop (A-F8) gives the
    /// lock up all the same, is filed with its error, and is not resumed:
    /// the pass goes on to the next target.
    #[test]
    fn a_target_whose_kernel_scrub_did_not_stop_is_filed_and_not_resumed() {
        let s = scratch();
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim);
        let (go, rx) = mpsc::channel();
        let ran = Arc::new(AtomicBool::new(false));
        let backup = backup(&s, rx, ran.clone(), Duration::from_millis(10));
        let calls = Mutex::new(Vec::new());
        let scrub_one = |_: &Config,
                         label: &str,
                         _: &dyn ProgressCallback,
                         _: &MaintenanceHeld,
                         claimed: &dyn Fn() -> bool| {
            calls
                .lock()
                .unwrap()
                .push((label.to_string(), ran.load(Ordering::SeqCst)));
            if label == "first" {
                go.send(()).unwrap();
                wait_until(Duration::from_secs(5), "the claim", claimed);
                let mut r = paused(label, false);
                r.errors.push("the kernel still reports a scrub".into());
                return r;
            }
            clean(label)
        };
        let f = filed();
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &[],
            &PassHooks {
                stop: &|| None,
                reload: &|| Ok(config()),
                scrub_one: &scrub_one,
                file_result: &|r| f.results.lock().unwrap().push(r.target_label.clone()),
                file_yield: &|m| f.yields.lock().unwrap().push(m.clone()),
            },
        );
        backup.join().unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [("first".to_string(), false), ("second".to_string(), true)]
        );
        assert_eq!(results.len(), 2);
        assert!(!results[0].ok());
        assert_eq!(results[0].yields.len(), 1);
        assert!(results[1].ok());
        assert_eq!(*f.results.lock().unwrap(), ["first", "second"]);
    }

    /// The `yielded` record: this process, this boot, the claimant, the
    /// count and the pause so far, the earlier success kept; a filesystem
    /// with no UUID is not filed.
    #[test]
    fn a_yielded_record_names_the_claimant_and_keeps_the_last_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub-state.json");
        let marker = RunningMarker {
            label: "first",
            fsuuid: "u-first",
            mountpoint: "/mnt/first",
            started_epoch: 1_000,
        };
        record_running(&path, &marker, &NullProgress).unwrap();
        let mut state = load_state_from(&path).unwrap();
        state
            .filesystems
            .get_mut("u-first")
            .unwrap()
            .last_success_epoch = Some(500);
        save_state_to(&state, &path).unwrap();
        let mark = YieldMark {
            label: "first".into(),
            fsuuid: "u-first".into(),
            mountpoint: "/mnt/first".into(),
            started_epoch: 1_000,
            at_epoch: 1_200,
            holder: "backup-run.sh pid 42".into(),
            yields: 2,
            paused_secs: 30,
        };
        record_yielded(&path, &mark, &NullProgress).unwrap();
        let fs = load_state_from(&path).unwrap().filesystems["u-first"].clone();
        assert_eq!(fs.last_success_epoch, Some(500));
        let a = &fs.last_attempt;
        assert_eq!(a.outcome, "yielded");
        assert!(!a.ok);
        assert_eq!(a.started_epoch, 1_000);
        assert_eq!(a.pid, Some(std::process::id()));
        assert_eq!(a.boot_id, current_boot_id());
        assert_eq!(a.yielded_to.as_deref(), Some("backup-run.sh pid 42"));
        assert_eq!(
            (a.yielded_epoch, a.yields, a.paused_secs),
            (Some(1_200), Some(2), Some(30))
        );
        assert!(a.is_live(), "a pause of this live pass is in progress");

        let unresolved = YieldMark {
            fsuuid: String::new(),
            ..mark
        };
        let before = std::fs::read_to_string(&path).unwrap();
        record_yielded(&path, &unresolved, &NullProgress).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// Checkpoint 1 at the pass's start: the scrub singleton is taken, but
    /// the maintenance lock is not while a backup claims it.
    #[test]
    fn a_pass_starting_while_a_backup_claims_lets_the_backup_go_first() {
        let s = scratch();
        let singleton = s.maintenance.with_file_name("scrub.lock");
        let claimed = Claim::take_at(&s.claim, "test backup").unwrap();
        let (singleton2, maintenance, claim) =
            (singleton.clone(), s.maintenance.clone(), s.claim.clone());
        let pass = std::thread::spawn(move || {
            matches!(
                acquire_locks_at(
                    &singleton2,
                    &maintenance,
                    &claim,
                    &NullProgress,
                    Duration::ZERO,
                    POLL
                )
                .unwrap(),
                ScrubLockAttempt::Taken(_)
            )
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(!pass.is_finished(), "the pass went ahead of the claim");
        assert!(
            lock_is_free(&s.maintenance),
            "the pass took the claimed lock"
        );
        drop(claimed);
        assert!(pass.join().unwrap());
    }

    // --- pause and resume by hand (bd DAS-Backup-Manager-su1b.15) ----------

    fn request_in(s: &Scratch) -> PathBuf {
        s.claim.with_file_name("das-scrub-pause")
    }

    /// The pause during a target: the segment stops for the claim the pause
    /// command takes, the pass sees the request in its wait and ends at once
    /// (the claim still held), filing the cut target `paused` — not a
    /// failure — and never scrubbing the next one.
    #[test]
    fn a_pause_request_during_a_target_ends_the_pass_paused_without_waiting() {
        let s = scratch();
        let request = request_in(&s);
        let claimed = Mutex::new(None);
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim).with_pause_request(&request);
        let calls = AtomicUsize::new(0);
        let scrub_one = |_: &Config,
                         label: &str,
                         _: &dyn ProgressCallback,
                         _: &MaintenanceHeld,
                         wants: &dyn Fn() -> bool| {
            calls.fetch_add(1, Ordering::SeqCst);
            assert!(!wants(), "nothing asked yet");
            // `btrdasd scrub pause`, mid-scrub: the request, then the claim.
            write_pause_request(&request, "tester pid 1", 1_000).unwrap();
            *claimed.lock().unwrap() =
                Some(Claim::take_at(&s.claim, "btrdasd scrub pause").unwrap());
            assert!(wants(), "the scrub sees the pause");
            paused(label, true)
        };
        let f = filed();
        let started = Instant::now();
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &[],
            &PassHooks {
                stop: &|| None,
                reload: &|| panic!("a paused pass reads nothing again"),
                scrub_one: &scrub_one,
                file_result: &|r| f.results.lock().unwrap().push(r.target_label.clone()),
                file_yield: &|m| f.yields.lock().unwrap().push(m.clone()),
            },
        );
        assert!(started.elapsed() < Duration::from_secs(2), "it waited");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the next target was begun");
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.target_label, "first");
        assert_eq!(
            r.paused.as_ref().map(|p| p.by.as_str()),
            Some("tester pid 1")
        );
        assert_eq!(r.status_word(), "PAUSED");
        assert!(r.scrub_launched);
        assert_eq!(r.fsuuid, "u-first");
        assert_eq!(*f.results.lock().unwrap(), ["first"]);
        assert_eq!(
            f.yields.lock().unwrap().len(),
            1,
            "the yield is recorded first"
        );
        assert!(
            slot.held().is_none(),
            "a paused pass holds no maintenance lock"
        );
        assert!(lock_is_free(&s.maintenance));
        let pass = ScrubPass {
            status: PassStatus::Paused,
            started_epoch: 0,
            finished_epoch: 0,
            results,
            errors: Vec::new(),
            already_scrubbed: Vec::new(),
        };
        assert_eq!(pass.failed_count(), 0, "a pause is not a failure");
        assert_eq!(pass.clean_count(), 0, "nor is it a clean scrub");
        assert!(pass.success());
        let report = format_scrub_report(&pass, &config());
        assert!(report.contains("Status: PAUSED"), "{report}");
        assert!(!report.contains("FAILURES"), "{report}");
        drop(claimed);
    }

    /// The request alone, with no claim (the pause command's bound ran out and
    /// it let its claim go): the pass still yields at its next look — here
    /// before the first mount — and ends paused, nothing scrubbed.
    #[test]
    fn a_pause_request_without_a_claim_pauses_at_the_next_checkpoint() {
        let s = scratch();
        let request = request_in(&s);
        write_pause_request(&request, "tester pid 2", 1_000).unwrap();
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim).with_pause_request(&request);
        let scrub_one = |_: &Config,
                         label: &str,
                         _: &dyn ProgressCallback,
                         _: &MaintenanceHeld,
                         _: &dyn Fn() -> bool|
         -> ScrubFsResult {
            panic!("{label} was mounted while a pause was asked for")
        };
        let f = filed();
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &[],
            &PassHooks {
                stop: &|| None,
                reload: &|| panic!("a paused pass reads nothing again"),
                scrub_one: &scrub_one,
                file_result: &|r| f.results.lock().unwrap().push(r.target_label.clone()),
                file_yield: &|m| f.yields.lock().unwrap().push(m.clone()),
            },
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].target_label, "first");
        assert!(results[0].paused.is_some());
        assert!(!results[0].scrub_launched);
        assert_eq!(results[0].fsuuid, "u-first", "filed under its UUID");
    }

    /// A paused result is filed `paused`: not in progress, not interrupted,
    /// naming who asked and when, the last success kept.
    #[test]
    fn a_paused_result_is_filed_paused_and_keeps_the_last_success() {
        let mut state = ScrubState::default();
        merge_result_into_state(&mut state, &clean("first"), 2_000);
        state
            .filesystems
            .get_mut("u-first")
            .unwrap()
            .last_success_epoch = Some(500);
        let mut r = paused("first", true);
        r.pending_yield = None;
        r.yields.push(ScrubYield {
            at_epoch: 1_100,
            holder: "btrdasd scrub pause pid 9".into(),
            paused_secs: None,
        });
        r.paused = Some(PausedBy {
            by: "btrdasd scrub pause pid 9".into(),
            at_epoch: 1_200,
        });
        merge_result_into_state(&mut state, &r, 2_000);
        let fs = &state.filesystems["u-first"];
        assert_eq!(fs.last_success_epoch, Some(500));
        let a = &fs.last_attempt;
        assert_eq!(a.outcome, "paused");
        assert!(!a.is_in_progress());
        assert_eq!(a.effective_outcome(), "paused");
        assert_eq!(a.yielded_to.as_deref(), Some("btrdasd scrub pause pid 9"));
        assert_eq!(a.yielded_epoch, Some(1_200));
        assert_eq!(a.yields, Some(1));
        assert_eq!(a.signal, None);
    }

    #[test]
    fn the_pause_request_file_is_root_s_alone_and_reads_back() {
        let s = scratch();
        let request = request_in(&s);
        assert_eq!(read_pause_request(&request), None);
        write_pause_request(&request, "btrdasd scrub pause pid 7, by user", 1_234).unwrap();
        let mode = fs::metadata(&request).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            read_pause_request(&request),
            Some(PauseRequest {
                by: "btrdasd scrub pause pid 7, by user".into(),
                epoch: Some(1_234),
            })
        );
        fs::write(&request, "garbage\n").unwrap();
        let odd = read_pause_request(&request).expect("an odd file is still a request");
        assert!(odd.by.contains("names nobody"), "{}", odd.by);
        assert!(remove_pause_request(&request).unwrap());
        assert!(!remove_pause_request(&request).unwrap());
    }

    /// A pass started while a request is in place does not run, removes
    /// nothing, and reads as paused, not as a failure to start.
    #[test]
    fn a_pass_started_while_paused_does_not_run_and_keeps_the_request() {
        let s = scratch();
        let request = request_in(&s);
        assert!(paused_at_start(&request, 1, &NullProgress).is_none());
        write_pause_request(&request, "tester", 1).unwrap();
        let progress = TestProgress::new();
        let pass = paused_at_start(&request, 1, &progress).expect("paused");
        assert_eq!(pass.status, PassStatus::Paused);
        assert!(pass.results.is_empty());
        assert!(request.exists(), "the request stays until resume");
    }

    fn pause_paths(s: &Scratch) -> PausePaths {
        PausePaths {
            singleton: s.claim.with_file_name("scrub.lock"),
            claim: s.claim.clone(),
            request: request_in(s),
            state: s.claim.with_file_name("scrub-state.json"),
        }
    }

    #[test]
    fn pause_with_no_pass_running_writes_nothing() {
        let s = scratch();
        let p = pause_paths(&s);
        assert_eq!(
            pause_pass_at(&p, "tester", Duration::from_secs(1), POLL).unwrap(),
            PauseOutcome::NotRunning
        );
        assert!(!p.request.exists());
    }

    #[test]
    fn pause_while_setup_holds_the_singleton_writes_nothing() {
        let s = scratch();
        let p = pause_paths(&s);
        let _setup = crate::backup::SingletonHeld::try_acquire_at(&p.singleton, "btrdasd setup")
            .unwrap()
            .unwrap();
        assert!(matches!(
            pause_pass_at(&p, "tester", Duration::from_secs(1), POLL).unwrap(),
            PauseOutcome::SetupHolds(_)
        ));
        assert!(!p.request.exists());
    }

    /// A pass that never answers: the pause gives up after its bound, names
    /// it, lets its claim go and leaves the request for the pass's next look.
    #[test]
    fn a_pass_that_does_not_answer_keeps_the_request_and_frees_the_claim() {
        let s = scratch();
        let p = pause_paths(&s);
        let _pass = crate::backup::SingletonHeld::try_acquire_at(&p.singleton, SCRUB_JOB)
            .unwrap()
            .unwrap();
        let outcome = pause_pass_at(&p, "tester", Duration::from_millis(100), POLL).unwrap();
        assert!(
            matches!(&outcome, PauseOutcome::NoAnswer(h) if h.contains(SCRUB_JOB)),
            "{outcome:?}"
        );
        assert!(p.request.exists());
        assert!(!claim_is_held(&p.claim).unwrap(), "the claim was let go");
    }

    /// The whole hand-off with a stand-in pass: it waits for the claim, files
    /// `paused`, lets the singleton go; the pause reports it and keeps the
    /// request. A pass that ends without pausing anything has the request
    /// taken away.
    #[test]
    fn pause_reports_the_paused_filesystem_once_the_pass_ends() {
        let s = scratch();
        let p = pause_paths(&s);
        for pauses in [true, false] {
            let held = crate::backup::SingletonHeld::try_acquire_at(&p.singleton, SCRUB_JOB)
                .unwrap()
                .unwrap();
            let (claim, state) = (p.claim.clone(), p.state.clone());
            let pass = std::thread::spawn(move || {
                wait_until(Duration::from_secs(5), "the claim", || {
                    claim_is_held(&claim).unwrap()
                });
                let mut st = ScrubState::default();
                let mut r = clean("first");
                if pauses {
                    r.outcome = None;
                    r.paused = Some(PausedBy {
                        by: "tester".into(),
                        at_epoch: 5,
                    });
                }
                merge_result_into_state(&mut st, &r, 10);
                save_state_to(&st, &state).unwrap();
                drop(held);
            });
            let outcome = pause_pass_at(&p, "tester", Duration::from_secs(5), POLL).unwrap();
            pass.join().unwrap();
            if pauses {
                assert_eq!(outcome, PauseOutcome::Paused(vec!["first".into()]));
                assert!(p.request.exists());
            } else {
                assert_eq!(outcome, PauseOutcome::EndedUnpaused);
                assert!(!p.request.exists());
            }
        }
    }

    #[test]
    fn resume_refuses_while_a_pass_runs_and_otherwise_lifts_and_starts() {
        let s = scratch();
        let p = pause_paths(&s);
        write_pause_request(&p.request, "tester", 1).unwrap();
        let starts = AtomicUsize::new(0);
        let start = || {
            starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let pass = crate::backup::SingletonHeld::try_acquire_at(&p.singleton, SCRUB_JOB)
            .unwrap()
            .unwrap();
        assert!(matches!(
            resume_pass_at(&p, &start).unwrap(),
            ResumeOutcome::AlreadyRunning(_)
        ));
        assert!(p.request.exists(), "nothing changed while it runs");
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        drop(pass);
        assert_eq!(
            resume_pass_at(&p, &start).unwrap(),
            ResumeOutcome::Started { removed: true }
        );
        assert!(!p.request.exists());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(resume_pass_at(&p, &|| Err("no systemd".into())).is_err());
    }

    // ---- bd DAS-Backup-Manager-su1b.24: resuming a paused pass ----

    /// A pass that ends paused, as `run_scrub_pass` builds it: `results` in
    /// order, the last one cut by the pause at `paused_at`.
    fn paused_pass(started: i64, mut results: Vec<ScrubFsResult>, paused_at: i64) -> ScrubPass {
        if let Some(last) = results.last_mut() {
            last.outcome = None;
            last.paused = Some(PausedBy {
                by: "btrdasd scrub pause pid 1, by user".into(),
                at_epoch: paused_at,
            });
        }
        ScrubPass {
            status: PassStatus::Paused,
            started_epoch: started,
            finished_epoch: paused_at,
            results,
            errors: Vec::new(),
            already_scrubbed: Vec::new(),
        }
    }

    /// The defect as it was met on the host 2026-11-23: the first target
    /// finished, a pause cut the second; `btrdasd scrub resume` started a
    /// pass that scrubbed the first again from zero. The resuming pass must
    /// leave the first out and begin at the second.
    #[test]
    fn a_pass_paused_after_its_first_target_resumes_at_the_second() {
        let mut state = ScrubState::default();
        merge_pass_into_state(
            &mut state,
            &paused_pass(10_000, vec![clean("first"), clean("second")], 11_000),
        );
        let skip = labels_already_scrubbed(&state, &config(), 11_500, &NullProgress);
        assert_eq!(skip, ["first"]);

        let s = scratch();
        let mut slot = slot_held(&s.maintenance);
        let watch = ClaimWatch::new(&s.claim);
        let calls = Mutex::new(Vec::new());
        let scrub_one = |_: &Config,
                         label: &str,
                         _: &dyn ProgressCallback,
                         _: &MaintenanceHeld,
                         _: &dyn Fn() -> bool| {
            calls.lock().unwrap().push(label.to_string());
            clean(label)
        };
        let results = scrub_targets_yielding(
            &config(),
            &NullProgress,
            &mut slot,
            &watch,
            &skip,
            &PassHooks {
                stop: &|| None,
                reload: &|| Ok(config()),
                scrub_one: &scrub_one,
                file_result: &|_| {},
                file_yield: &|_| panic!("nothing claimed"),
            },
        );
        assert_eq!(*calls.lock().unwrap(), ["second"]);
        assert_eq!(results.len(), 1);
    }

    /// Paused twice: the second part's marker still holds what the first
    /// part finished, from the first part's start.
    #[test]
    fn a_pass_paused_twice_still_skips_what_its_first_part_finished() {
        let mut state = ScrubState::default();
        merge_pass_into_state(
            &mut state,
            &paused_pass(10_000, vec![clean("first"), clean("second")], 11_000),
        );
        // The resumed part scrubs only `second`, and is paused again.
        merge_pass_into_state(
            &mut state,
            &paused_pass(11_500, vec![clean("second")], 12_000),
        );
        let marker = state.paused_pass.clone().unwrap();
        assert_eq!(marker.origin_epoch, 10_000);
        assert_eq!(marker.paused_epoch, 12_000);
        assert_eq!(marker.finished, ["u-first"]);
        assert_eq!(
            labels_already_scrubbed(&state, &config(), 12_500, &NullProgress),
            ["first"]
        );
    }

    /// Past the window a pass scrubs everything again (a reboot takes the
    /// pause request away; the next scheduled pass is a new one), and a pass
    /// that runs to its end leaves no marker.
    #[test]
    fn a_stale_marker_or_a_completed_pass_skips_nothing() {
        let mut state = ScrubState::default();
        merge_pass_into_state(
            &mut state,
            &paused_pass(10_000, vec![clean("first"), clean("second")], 11_000),
        );
        let late = 11_000 + RESUME_WINDOW_SECS + 1;
        assert!(labels_already_scrubbed(&state, &config(), late, &NullProgress).is_empty());
        assert!(
            !labels_already_scrubbed(
                &state,
                &config(),
                11_000 + RESUME_WINDOW_SECS,
                &NullProgress
            )
            .is_empty()
        );
        // A pass started before the pause (a clock gone back) is no resumption.
        assert!(labels_already_scrubbed(&state, &config(), 10_999, &NullProgress).is_empty());

        let mut done = paused_pass(11_500, vec![clean("second")], 12_000);
        done.status = PassStatus::Completed;
        done.results[0].paused = None;
        done.results[0].outcome = Some(ScrubOutcome::Finished);
        merge_pass_into_state(&mut state, &done);
        assert!(state.paused_pass.is_none());
        assert!(labels_already_scrubbed(&state, &config(), 12_500, &NullProgress).is_empty());
    }

    /// Only a scrub the kernel finished counts: one finished with findings is
    /// done (its findings went out with that pass's report); one that
    /// produced no record, or was canceled, is scrubbed again.
    #[test]
    fn only_a_finished_scrub_is_skipped_on_resume() {
        let mut with_findings = clean("first");
        with_findings.counters.corrected_errors = 7;
        let mut state = ScrubState::default();
        merge_pass_into_state(
            &mut state,
            &paused_pass(10_000, vec![with_findings, clean("second")], 11_000),
        );
        assert_eq!(
            labels_already_scrubbed(&state, &config(), 11_500, &NullProgress),
            ["first"]
        );

        for outcome in [
            None,
            Some(ScrubOutcome::Canceled),
            Some(ScrubOutcome::Aborted),
        ] {
            let mut failed = clean("first");
            failed.outcome = outcome;
            let mut state = ScrubState::default();
            merge_pass_into_state(
                &mut state,
                &paused_pass(10_000, vec![failed, clean("second")], 11_000),
            );
            assert!(
                labels_already_scrubbed(&state, &config(), 11_500, &NullProgress).is_empty(),
                "{outcome:?} counted as scrubbed"
            );
        }
    }

    /// The marker survives the state file's round trip, and an older file
    /// without it reads as no marker.
    #[test]
    fn the_paused_pass_marker_round_trips_and_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub-state.json");
        let mut state = ScrubState::default();
        merge_pass_into_state(
            &mut state,
            &paused_pass(10_000, vec![clean("first"), clean("second")], 11_000),
        );
        save_state_to(&state, &path).unwrap();
        assert_eq!(
            load_state_from(&path).unwrap().paused_pass,
            state.paused_pass
        );
        std::fs::write(&path, r#"{"schema_version":1,"filesystems":{}}"#).unwrap();
        assert!(load_state_from(&path).unwrap().paused_pass.is_none());
    }

    /// The report names what the resuming pass left out, so "1 filesystem
    /// scrubbed" does not read as the others forgotten.
    #[test]
    fn the_report_lists_what_a_resumed_pass_did_not_scrub_again() {
        let mut pass = paused_pass(11_500, vec![clean("second")], 12_000);
        pass.status = PassStatus::Completed;
        pass.results[0].paused = None;
        pass.results[0].outcome = Some(ScrubOutcome::Finished);
        pass.already_scrubbed = vec!["first".into()];
        let report = format_scrub_report(&pass, &config());
        assert!(report.contains("Already scrubbed      1"), "{report}");
        assert!(
            report
                .lines()
                .any(|l| l.trim_start().starts_with("first") && l.contains("DONE")),
            "{report}"
        );
        assert!(!report.contains("PROBLEMS"), "{report}");
    }
}
