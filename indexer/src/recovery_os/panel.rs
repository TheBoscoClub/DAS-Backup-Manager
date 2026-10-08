//! The recovery-drive panel's rules and its status document (bd 8249 stage 2).
//! Pure: everything the system says comes through [`PanelReads`].
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{Config, Target, TargetRole};
use crate::recovery_os::{
    Assessment, BootVerdict, GuestAgent, HostVersions, RecoveryOs, StoredDrive, StoredState,
    assess, newest_kernel,
};

/// Consecutive clean unattended runs, on BOTH drives, before parallel is the
/// everyday choice (decision 7).
pub const CLEAN_RUNS_FOR_PARALLEL: u32 = 3;
/// The first boot-record schema that carries the guest-agent reading.
pub const SCHEMA_FOR_UNATTENDED: u32 = 4;
/// History lines the panel shows per drive.
const HISTORY_SHOWN: usize = 20;

/// What `systemctl show` and the unit file say about one unit. A missing
/// reading is `None`, never 0 or empty.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnitFacts {
    pub exists: bool,
    pub active_state: String,
    pub next_elapse_epoch: Option<i64>,
    pub last_trigger_epoch: Option<i64>,
    /// The service's `Result=`.
    pub result: Option<String>,
    pub exec_main_status: Option<i32>,
    pub exec_main_start_epoch: Option<i64>,
    /// Read from the unit file.
    pub on_calendar: Option<String>,
    /// The service's `ExecStart=`, read from the unit file.
    pub exec_start: Option<String>,
    /// The last 5 journal lines.
    pub last_journal: Vec<String>,
}

/// A scheduled session (filled in by the schedule layer).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Schedule {
    pub unit: String,
    /// The `OnCalendar=` time read back; `None` when it cannot be read.
    pub at_epoch: Option<i64>,
    pub mode: Option<String>,
    /// pending | missed | fired | running
    pub state: String,
    pub detail: String,
}

/// A session holding the drive now (filled in by the schedule layer).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Session {
    /// `job:<id>` | `unit:<name>` | `other:<holder line>`
    pub by: String,
    pub since_epoch: Option<i64>,
    pub domain_state: Option<String>,
    pub attended: Option<bool>,
}

/// What [`status_json`] reads from the system: the helper implements it with
/// the script and systemctl, tests with a scripted one.
pub trait PanelReads {
    /// `recovery-os-vm.sh clean-runs <label>`.
    fn clean_runs(&self, label: &str) -> Result<u32, String>;
    /// `recovery-os-vm.sh history <label>`, newest last.
    fn history(&self, label: &str) -> Result<Vec<Value>, String>;
    fn unit(&self, name: &str) -> Result<UnitFacts, String>;
    /// `virsh domstate recovery-os-updater-<label>`; `None` when virsh cannot say.
    fn domain_state(&self, label: &str) -> Option<String>;
    /// First line of `/run/das-maintenance.lock` if held, else `None`.
    fn lock_holder(&self) -> Option<String>;
    /// The id of the helper's running recovery-OS session job, if any.
    fn running_job(&self) -> Option<String>;
}

fn agent_words(a: &GuestAgent) -> String {
    match a {
        GuestAgent::Read {
            installed: true,
            enabled,
            ..
        } => {
            if *enabled {
                "installed, enabled".to_string()
            } else {
                "installed, not enabled".to_string()
            }
        }
        GuestAgent::Read { .. } => "not installed".to_string(),
        GuestAgent::Unreadable { reason } => format!("unreadable: {reason}"),
    }
}

/// Whether `label`'s record admits an unattended session, by the script's own
/// rule: schema 4, a reading (not an error), the guest agent installed and
/// started at boot, verdict not `will`, and the record's mount_uuid equal to
/// the target's. `Err` carries the one sentence the GUI shows.
pub fn unattended_possible(
    target: &Target,
    state: &StoredState,
    drive: Option<&StoredDrive>,
) -> Result<(), String> {
    if state.schema_version < SCHEMA_FOR_UNATTENDED {
        return Err(format!(
            "the boot record is schema {}; an unattended session needs schema 4 (let a backup run record the drive again)",
            state.schema_version
        ));
    }
    let Some(target_uuid) = target.mount_uuid.as_deref() else {
        return Err(format!(
            "{} has no mount_uuid in config.toml; the session cannot tie the record to the filesystem",
            target.label
        ));
    };
    let Some(d) = drive else {
        return Err(format!(
            "no record of {} (let a backup run record it)",
            target.label
        ));
    };
    if let Some(e) = &d.error {
        return Err(format!("the last reading of {} failed: {e}", target.label));
    }
    let Some(os) = &d.os else {
        return Err(format!("the record of {} holds no reading", target.label));
    };
    match d.mount_uuid.as_deref() {
        Some(u) if u == target_uuid => {}
        Some(u) => {
            return Err(format!(
                "the record of {} is of filesystem {u}, config mounts {target_uuid}",
                target.label
            ));
        }
        None => {
            return Err(format!(
                "the record of {} names no filesystem; let a backup run record it again",
                target.label
            ));
        }
    }
    if !os.guest_agent.runs_at_boot() {
        return Err(format!(
            "the guest agent does not start at boot in {}'s OS ({}); run an attended session and install/enable qemu-guest-agent",
            target.label,
            agent_words(&os.guest_agent)
        ));
    }
    if os.btrbk_at_boot.verdict == BootVerdict::Will {
        return Err(format!(
            "{}'s OS will run btrbk at boot; an attended session must disable it first",
            target.label
        ));
    }
    Ok(())
}

/// Parallel is the everyday choice once BOTH drives have at least
/// [`CLEAN_RUNS_FOR_PARALLEL`] consecutive clean unattended runs; `None` =
/// count unknown, which is not enough.
pub fn pair_mode_default(clean_runs: &[Option<u32>]) -> &'static str {
    if clean_runs.len() == 2
        && clean_runs
            .iter()
            .all(|c| c.is_some_and(|n| n >= CLEAN_RUNS_FOR_PARALLEL))
    {
        "parallel"
    } else {
        "sequential"
    }
}

/// A session is due when the OS is stale by age; an unknown age is reported
/// stale, not scheduled.
pub fn due(a: &Assessment, max_age_days: u32) -> bool {
    a.age_days.is_some_and(|d| d >= i64::from(max_age_days))
}

/// One drive as the panel shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct DriveStatus {
    pub label: String,
    pub display_name: String,
    pub serials: Vec<String>,
    pub record: Option<Value>,
    pub checked_epoch: Option<i64>,
    pub record_error: Option<String>,
    pub assessment: Option<Assessment>,
    pub due: bool,
    pub verdict: Option<String>,
    pub unattended: Result<(), String>,
    pub clean_runs: Option<u32>,
    pub clean_runs_error: Option<String>,
    pub history: Vec<Value>,
    pub history_error: Option<String>,
    pub schedule: Option<Schedule>,
    /// Why this drive's schedule units could not be read.
    pub schedule_error: Option<String>,
    pub session: Option<Session>,
    /// Why a schedule service that could hold this drive could not be read.
    pub session_error: Option<String>,
}

impl DriveStatus {
    /// The GUI's document for this drive; `unattended` is `{possible, why}`.
    pub fn to_json(&self) -> Value {
        json!({
            "label": self.label,
            "display_name": self.display_name,
            "serials": self.serials,
            "checked_epoch": self.checked_epoch,
            "record_error": self.record_error,
            "record": self.record,
            "assessment": self.assessment,
            "due": self.due,
            "verdict": self.verdict,
            "unattended": {
                "possible": self.unattended.is_ok(),
                "why": self.unattended.as_ref().err().cloned().unwrap_or_default(),
            },
            "clean_runs": self.clean_runs,
            "clean_runs_error": self.clean_runs_error,
            "history": self.history,
            "history_error": self.history_error,
            "schedule": self.schedule,
            "schedule_error": self.schedule_error,
            "session": self.session,
            "session_error": self.session_error,
        })
    }
}

fn verdict_word(v: BootVerdict) -> &'static str {
    match v {
        BootVerdict::Will => "will",
        BootVerdict::May => "may",
        BootVerdict::No => "no",
    }
}

/// The trimmed reading the panel shows; a version not read is `null`.
fn record_value(os: &RecoveryOs, host: &HostVersions) -> Value {
    json!({
        "os": os.os_name,
        "installed": os.installed,
        "last_full_upgrade": os.last_full_upgrade_applied,
        "kernel": newest_kernel(&os.kernels),
        "host_kernel": host.kernel,
        "btrfs_progs": os.packages.get("btrfs-progs"),
        "host_btrfs_progs": host.btrfs_progs,
        "btrbk": os.packages.get("btrbk"),
        "guest_agent": os.guest_agent,
    })
}

/// What every drive's status shares: the clock, the lock's holder, the
/// helper's session job.
struct Now<'a> {
    epoch: i64,
    holder: Option<&'a str>,
    job: Option<&'a str>,
}

fn drive_status(
    cfg: &Config,
    target: &Target,
    state: &Result<Option<StoredState>, String>,
    host: &HostVersions,
    today: &str,
    now: &Now<'_>,
    reads: &dyn PanelReads,
) -> DriveStatus {
    let max_age = cfg.recovery_os.max_age_days;
    let (drive, unattended, mut record_error) = match state {
        Err(e) => (
            None,
            Err(format!("the boot record cannot be read: {e}")),
            Some(e.clone()),
        ),
        Ok(None) => (
            None,
            Err("there is no boot record yet (let a backup run record the drives)".to_string()),
            Some("there is no boot record yet".to_string()),
        ),
        Ok(Some(s)) => {
            let d = s.drives.get(&target.label);
            (d, unattended_possible(target, s, d), None)
        }
    };
    let os = drive.and_then(|d| d.os.as_ref());
    if state.as_ref().is_ok_and(Option::is_some) {
        record_error = match drive {
            None => Some(format!("no record of {}", target.label)),
            Some(d) => d.error.clone(),
        };
    }
    let assessment = os.map(|os| assess(os, host, today, max_age));
    let (clean_runs, clean_runs_error) = match reads.clean_runs(&target.label) {
        Ok(n) => (Some(n), None),
        Err(e) => (None, Some(e)),
    };
    let (history, history_error) = match reads.history(&target.label) {
        Ok(h) => (h, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let skip = history.len().saturating_sub(HISTORY_SHOWN);
    let (schedule, schedule_error) = schedule_of(
        &format!("{UNIT_PREFIX}{}", target.label),
        history.last(),
        now.epoch,
        reads,
    );
    let (session, session_error) = session_of(&target.label, now.holder, now.job, reads);
    DriveStatus {
        label: target.label.clone(),
        display_name: target.display_name.clone(),
        serials: target.effective_serials(),
        record: os.map(|os| record_value(os, host)),
        checked_epoch: drive.map(|d| d.checked_epoch),
        record_error,
        due: assessment.as_ref().is_some_and(|a| due(a, max_age)),
        verdict: os.map(|os| verdict_word(os.btrbk_at_boot.verdict).to_string()),
        assessment,
        unattended,
        clean_runs,
        clean_runs_error,
        history: history.into_iter().skip(skip).collect(),
        history_error,
        schedule,
        schedule_error,
        session,
        session_error,
    }
}

/// The document the GUI renders: every `role = "mirror"` target in config
/// order, each with its record, verdict, reasons, history, schedule and
/// session; and the pair's schedule and session (the `…-both` units).
/// `now_epoch` is the clock schedules are judged by.
pub fn status_json(
    cfg: &Config,
    state: &Result<Option<StoredState>, String>,
    host: &HostVersions,
    today: &str,
    now_epoch: i64,
    reads: &dyn PanelReads,
) -> Value {
    let holder = reads.lock_holder();
    let job = reads.running_job();
    let now = Now {
        epoch: now_epoch,
        holder: holder.as_deref(),
        job: job.as_deref(),
    };
    let drives: Vec<DriveStatus> = cfg
        .targets
        .iter()
        .filter(|t| t.role == TargetRole::Mirror)
        .map(|t| drive_status(cfg, t, state, host, today, &now, reads))
        .collect();
    // The pair's newest history line is the later-starting of the two
    // drives' newest.
    let pair_newest = drives
        .iter()
        .filter_map(|d| d.history.last())
        .max_by_key(|h| h.get("start").and_then(Value::as_i64));
    let both = format!("{UNIT_PREFIX}both");
    let (pair_schedule, pair_schedule_error) = schedule_of(&both, pair_newest, now_epoch, reads);
    let (pair_session, pair_session_error) = match running_service(&both, reads) {
        Ok(found) => (
            found.map(|(name, facts)| Session {
                by: format!("unit:{name}"),
                since_epoch: facts.exec_main_start_epoch,
                domain_state: None,
                attended: Some(false),
            }),
            None,
        ),
        Err(e) => (None, Some(e)),
    };
    let mode_default = if drives.len() == 2 {
        pair_mode_default(&[drives[0].clean_runs, drives[1].clean_runs])
    } else {
        "sequential"
    };
    json!({
        "schema": 1,
        "max_age_days": cfg.recovery_os.max_age_days,
        "today": today,
        "pair": {
            "mode_default": mode_default,
            "schedule": pair_schedule,
            "schedule_error": pair_schedule_error,
            "session": pair_session,
            "session_error": pair_session_error,
        },
        "drives": drives.iter().map(DriveStatus::to_json).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Schedules: generated unit pairs (bd 8249 stage 2, Task 6)
// ---------------------------------------------------------------------------

/// Every schedule unit's name starts with this.
pub const UNIT_PREFIX: &str = "das-recovery-os-update-";
/// Where the helper writes the schedule units, and uninstall removes them.
pub const UNIT_DIR: &str = "/etc/systemd/system";

/// The lock wait every scheduled session asks of the script, in MINUTES
/// (`--wait-lock <minutes>`): 3 hours, room for a backup that overran.
const SCHEDULE_WAIT_LOCK_MIN: u32 = 180;
/// How far in the future a schedule must be, in seconds: time for the
/// helper to write, reload and arm it before it is due.
pub const SCHEDULE_MIN_LEAD: i64 = 120;

/// The unit name without its suffix: one label is that drive's schedule,
/// two are the pair's (`…-both`).
pub fn unit_base(labels: &[String]) -> String {
    match labels {
        [one] => format!("{UNIT_PREFIX}{one}"),
        _ => format!("{UNIT_PREFIX}both"),
    }
}

/// The service a schedule runs: `<script> session <labels> [--mode m]
/// --unattended --wait-lock 180`, a oneshot whose every reported outcome is a
/// success to systemd, so cachyos-sentinel never sees it `failed` and never
/// restarts it.
///
/// This unit, not the helper, is what keeps a schedule from colliding with a
/// session already running (a Now session, or the other drive's schedule):
/// the helper cannot see a timer fire, but the script it starts takes the
/// maintenance lock, waits at most 180 minutes (3 hours) for it
/// (`--wait-lock 180`) and refuses — exit 1, written to its journal — when
/// it is still held.
pub fn render_service(labels: &[String], mode: Option<&str>, script: &Path) -> String {
    let mut exec = format!("{} session {}", script.display(), labels.join(" "));
    if let Some(m) = mode {
        exec.push_str(&format!(" --mode {m}"));
    }
    exec.push_str(&format!(
        " --unattended --wait-lock {SCHEDULE_WAIT_LOCK_MIN}"
    ));
    format!(
        "[Unit]
Description=Scheduled update of the recovery drive OS: {labels}
Documentation=man:btrdasd(1)
Wants=network-online.target
After=network-online.target libvirtd.service
# Written by btrdasd-helper (RecoveryOsScheduleSet); removed when cleared or by uninstall.

[Service]
Type=oneshot
# Every outcome the driver reports (1 refused, 3/4 kept, 5 warnings, 6 guard, 7 stopped) travels by
# its history, summary and journal. None may leave this unit failed: cachyos-sentinel restarts
# failed units, and a restarted VM session is what must never happen unasked.
SuccessExitStatus=1 3 4 5 6 7
ExecStart={exec}
",
        labels = labels.join(" ")
    )
}

/// The one-shot timer for a schedule at `at_epoch`, written in the host's
/// local time (systemd reads a zone-less `OnCalendar=` as local).
/// `Persistent=false`: a time that passed while the host was off is
/// `missed`, never run at the next boot. `at_epoch` must be one
/// [`validate_schedule`] accepted (it refuses a time the C library cannot
/// write as a local time).
pub fn render_timer(labels: &[String], at_epoch: i64) -> String {
    let local = crate::caldate::local_datetime(at_epoch)
        .expect("validate_schedule refuses an epoch that has no local time");
    format!(
        "[Unit]
Description=Scheduled update of the recovery drive OS: {labels}
# Written by btrdasd-helper (RecoveryOsScheduleSet); removed when cleared or by uninstall.

[Timer]
OnCalendar={local}
Persistent=false
Unit={base}.service

[Install]
WantedBy=timers.target
",
        labels = labels.join(" "),
        base = unit_base(labels)
    )
}

/// The `--mode` an `ExecStart=` passes, if any.
fn mode_of(exec_start: &str) -> Option<String> {
    let mut words = exec_start.split_whitespace();
    words.find(|w| *w == "--mode")?;
    words.next().map(str::to_string)
}

/// `epoch` as local time for a sentence; the epoch itself when it has none.
fn local_words(epoch: i64) -> String {
    crate::caldate::local_datetime(epoch).unwrap_or_else(|| format!("epoch {epoch}"))
}

/// What a schedule's unit pair says, in the precedence that is true of it:
/// **running** (the service is active, activating, deactivating or
/// reloading); **pending** (the timer has a next elapse — a schedule set
/// again after it fired keeps its old last trigger, so this comes before
/// fired); **fired** (the timer triggered: the detail is the newest history
/// line's `<outcome> (exit <n>)` when that line started at or after the
/// trigger, else `refused: <the service's last journal lines>`, since a
/// session that wrote no history line never took the lock); **missed**
/// (anything else: the time passed, or the timer was stopped, without a
/// trigger). `unit` is left for the caller to name.
pub fn schedule_state(
    timer: &UnitFacts,
    service: &UnitFacts,
    history_newest: Option<&Value>,
    now_epoch: i64,
) -> Schedule {
    let at_epoch = timer
        .on_calendar
        .as_deref()
        .and_then(crate::caldate::local_epoch);
    let when = timer
        .on_calendar
        .clone()
        .unwrap_or_else(|| "a time that cannot be read from the timer".to_string());
    let (state, detail) = if matches!(
        service.active_state.as_str(),
        "active" | "activating" | "deactivating" | "reloading"
    ) {
        let since = match service.exec_main_start_epoch {
            Some(e) => format!("since {}", local_words(e)),
            None => "(its start time cannot be read)".to_string(),
        };
        (
            "running",
            format!("The scheduled session is running {since}."),
        )
    } else if let Some(next) = timer.next_elapse_epoch {
        let due = if next <= now_epoch { " (due now)" } else { "" };
        (
            "pending",
            format!(
                "The session will run unattended at {}{due}.",
                local_words(next)
            ),
        )
    } else if let Some(trigger) = timer.last_trigger_epoch {
        let own_line = history_newest.filter(|h| {
            h.get("start")
                .and_then(Value::as_i64)
                .is_some_and(|start| start >= trigger)
        });
        let detail = match own_line {
            Some(h) => {
                let outcome = h
                    .get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or("an outcome the history does not name");
                match h.get("exit").and_then(Value::as_i64) {
                    Some(code) => format!("{outcome} (exit {code})"),
                    None => format!("{outcome} (exit status not recorded)"),
                }
            }
            None => {
                let journal = if service.last_journal.is_empty() {
                    "the journal holds no line from it".to_string()
                } else {
                    service.last_journal.join(" / ")
                };
                let result = service.result.as_deref().unwrap_or("unknown");
                let status = service
                    .exec_main_status
                    .map_or_else(|| "unknown".to_string(), |s| s.to_string());
                format!(
                    "refused: {journal} (fired at {}; the session wrote no history line; unit result {result}, exit status {status})",
                    local_words(trigger)
                )
            }
        };
        ("fired", detail)
    } else if timer.active_state == "active" {
        (
            "missed",
            format!(
                "The time {when} passed without the timer firing (the host was off, or asleep); it will not run until it is scheduled again."
            ),
        )
    } else {
        (
            "missed",
            format!(
                "The timer for {when} is {} and never fired; it will not run until it is scheduled again.",
                if timer.active_state.is_empty() {
                    "in a state systemctl did not name"
                } else {
                    timer.active_state.as_str()
                }
            ),
        )
    };
    Schedule {
        unit: String::new(),
        at_epoch,
        mode: service.exec_start.as_deref().and_then(mode_of),
        state: state.to_string(),
        detail,
    }
}

/// Whether a schedule for `labels` at `at_epoch` may be written. Refused:
/// anything [`SessionRequest::validate`](super::session::SessionRequest::validate)
/// refuses; a label that cannot name a unit (only `A-Za-z0-9._-`, and not
/// `both`); a time less than [`SCHEDULE_MIN_LEAD`] seconds ahead, or one with
/// no local time; any drive whose record does not admit an unattended
/// session ([`unattended_possible`], with its reason); and a drive already in
/// the other kind of schedule — `existing` holds the unit bases that exist
/// now; the same kind is replaced, not refused.
pub fn validate_schedule(
    cfg: &Config,
    state: &StoredState,
    labels: &[String],
    at_epoch: i64,
    mode: Option<&str>,
    now_epoch: i64,
    existing: &[String],
) -> Result<(), String> {
    super::session::SessionRequest {
        labels: labels.to_vec(),
        unattended: true,
        mode: mode.map(str::to_string),
        accept_boot_record_risk: false,
    }
    .validate(cfg)?;
    for label in labels {
        if label == "both"
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(format!(
                "{label} cannot name a schedule unit (only letters, digits, '.', '_' and '-', and not \"both\")"
            ));
        }
    }
    if at_epoch < now_epoch + SCHEDULE_MIN_LEAD {
        return Err(format!(
            "the time must be in the future by at least {SCHEDULE_MIN_LEAD} seconds ({} is {} seconds from now)",
            local_words(at_epoch),
            at_epoch - now_epoch
        ));
    }
    if crate::caldate::local_datetime(at_epoch).is_none() {
        return Err(format!(
            "epoch {at_epoch} cannot be written as a local time"
        ));
    }
    for label in labels {
        let Some(target) = cfg.targets.iter().find(|t| &t.label == label) else {
            return Err(format!("{label} is not in the configuration"));
        };
        unattended_possible(target, state, state.drives.get(label))
            .map_err(|why| format!("a scheduled session is always unattended, and {why}"))?;
    }
    let both = format!("{UNIT_PREFIX}both");
    if labels.len() == 1 {
        if existing.contains(&both) {
            return Err(format!(
                "{} is already in the schedule for both drives ({both}); clear it first",
                labels[0]
            ));
        }
    } else {
        for label in labels {
            let own = format!("{UNIT_PREFIX}{label}");
            if existing.contains(&own) {
                return Err(format!(
                    "{label} already has a schedule of its own ({own}); clear it first"
                ));
            }
        }
    }
    Ok(())
}

/// One schedule as the panel shows it: `(schedule, error)`. No timer unit is
/// no schedule, and that is a reading; a unit that cannot be read is an
/// error, never "no schedule".
fn schedule_of(
    base: &str,
    history_newest: Option<&Value>,
    now_epoch: i64,
    reads: &dyn PanelReads,
) -> (Option<Schedule>, Option<String>) {
    let timer_name = format!("{base}.timer");
    let timer = match reads.unit(&timer_name) {
        Ok(t) => t,
        Err(e) => return (None, Some(format!("{timer_name} cannot be read: {e}"))),
    };
    if !timer.exists {
        return (None, None);
    }
    let service_name = format!("{base}.service");
    let service = match reads.unit(&service_name) {
        Ok(s) => s,
        Err(e) => return (None, Some(format!("{service_name} cannot be read: {e}"))),
    };
    let mut s = schedule_state(&timer, &service, history_newest, now_epoch);
    s.unit = timer_name;
    (Some(s), None)
}

/// A schedule service that is running now: `(unit, facts)`; `Ok(None)` when
/// it is not (or does not exist). A unit that cannot be read is an error,
/// never "not running".
fn running_service(
    base: &str,
    reads: &dyn PanelReads,
) -> Result<Option<(String, UnitFacts)>, String> {
    let name = format!("{base}.service");
    let facts = reads
        .unit(&name)
        .map_err(|e| format!("{name} cannot be read: {e}"))?;
    Ok(matches!(facts.active_state.as_str(), "active" | "activating").then_some((name, facts)))
}

/// Who holds `label` now. A schedule service covering it (its own or the
/// pair's) that is active or activating is `unit:<name>` — unless the
/// helper's session job is running and the lock names this drive, when it is
/// `job:<id>` (a timer that fires during a Now session waits on the lock,
/// it does not hold the drive). A lock line `recovery-os VM session <label>
/// …` with neither is `other:<line>`. A job without the lock naming a drive
/// is not attributed to one: the helper does not keep a job's labels.
///
/// The second value says which schedule service could not be read; it is
/// reported whatever else was found, since an unreadable unit may be the
/// one that holds the drive.
fn session_of(
    label: &str,
    holder: Option<&str>,
    job: Option<&str>,
    reads: &dyn PanelReads,
) -> (Option<Session>, Option<String>) {
    let held_here = holder.is_some_and(|h| {
        h.strip_prefix("recovery-os VM session ")
            .and_then(|rest| rest.split_whitespace().next())
            == Some(label)
    });
    let mut errors = Vec::new();
    let mut unit = None;
    for base in [
        format!("{UNIT_PREFIX}{label}"),
        format!("{UNIT_PREFIX}both"),
    ] {
        match running_service(&base, reads) {
            Ok(Some(found)) if unit.is_none() => unit = Some(found),
            Ok(_) => {}
            Err(e) => errors.push(e),
        }
    }
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    if held_here && let Some(id) = job {
        let s = Session {
            by: format!("job:{id}"),
            since_epoch: None,
            domain_state: reads.domain_state(label),
            attended: None,
        };
        return (Some(s), error);
    }
    if let Some((name, facts)) = unit {
        let s = Session {
            by: format!("unit:{name}"),
            since_epoch: facts.exec_main_start_epoch,
            domain_state: reads.domain_state(label),
            attended: Some(false),
        };
        return (Some(s), error);
    }
    let other = held_here.then(|| Session {
        by: format!("other:{}", holder.unwrap_or_default()),
        since_epoch: None,
        domain_state: reads.domain_state(label),
        attended: None,
    });
    (other, error)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Retention;
    use std::collections::HashMap;

    const UUID_A: &str = "60b05268-7f8f-47b5-a38a-752576a1172a";

    fn uuid_a() -> String {
        UUID_A.to_string()
    }

    fn mirror_target(label: &str, uuid: Option<&str>) -> Target {
        Target {
            label: label.to_string(),
            serial: String::new(),
            serials: vec![format!("SER-{label}")],
            mount_uuid: uuid.map(str::to_string),
            mount: format!("/mnt/{label}"),
            role: TargetRole::Mirror,
            retention: Retention::default(),
            display_name: format!("Drive {label}"),
        }
    }

    fn agent_at_boot() -> GuestAgent {
        GuestAgent::Read {
            installed: true,
            enabled: true,
            why: "started by udev".into(),
        }
    }

    fn agent_not_installed() -> GuestAgent {
        GuestAgent::Read {
            installed: false,
            enabled: false,
            why: "package absent".into(),
        }
    }

    fn stored_drive(agent: GuestAgent, verdict: BootVerdict, uuid: Option<String>) -> StoredDrive {
        let mut os = RecoveryOs {
            guest_agent: agent,
            kernels: vec!["6.17.1-1-cachyos".into()],
            log_read: true,
            installed: Some("2026-01-01".into()),
            last_full_upgrade_applied: Some("2026-09-20".into()),
            ..Default::default()
        };
        os.btrbk_at_boot.verdict = verdict;
        StoredDrive {
            checked_epoch: 1_791_448_102,
            mount_uuid: uuid,
            os: Some(os),
            error: None,
        }
    }

    fn state_v(schema: u32, drives: &[(&str, StoredDrive)]) -> StoredState {
        StoredState {
            schema_version: schema,
            drives: drives
                .iter()
                .map(|(l, d)| (l.to_string(), d.clone()))
                .collect(),
        }
    }

    pub(crate) fn two_mirrors_and_a_primary() -> Config {
        let mut cfg = Config::default();
        let mut primary = mirror_target("primary-22tb", None);
        primary.role = TargetRole::Primary;
        cfg.targets = vec![
            primary,
            mirror_target("system-recovery-A-2tb", Some(UUID_A)),
            mirror_target(
                "system-recovery-B-2tb",
                Some("7c7ae72d-09d6-4086-b249-1ac60f21b73b"),
            ),
        ];
        cfg
    }

    fn host() -> HostVersions {
        HostVersions {
            kernel: Some("6.17.2-1-cachyos".into()),
            btrfs_progs: Some("6.17-1".into()),
        }
    }

    #[derive(Default)]
    struct Scripted {
        clean: HashMap<&'static str, Result<u32, String>>,
        history: HashMap<&'static str, Result<Vec<Value>, String>>,
        /// Unit name -> facts; a unit not listed does not exist.
        units: HashMap<String, Result<UnitFacts, String>>,
        lock: Option<String>,
        domain: Option<String>,
        job: Option<String>,
    }

    impl PanelReads for Scripted {
        fn clean_runs(&self, label: &str) -> Result<u32, String> {
            self.clean
                .get(label)
                .cloned()
                .unwrap_or_else(|| Err("not scripted".into()))
        }
        fn history(&self, label: &str) -> Result<Vec<Value>, String> {
            self.history
                .get(label)
                .cloned()
                .unwrap_or_else(|| Ok(Vec::new()))
        }
        fn unit(&self, name: &str) -> Result<UnitFacts, String> {
            self.units
                .get(name)
                .cloned()
                .unwrap_or_else(|| Ok(UnitFacts::default()))
        }
        fn domain_state(&self, _label: &str) -> Option<String> {
            self.domain.clone()
        }
        fn lock_holder(&self) -> Option<String> {
            self.lock.clone()
        }
        fn running_job(&self) -> Option<String> {
            self.job.clone()
        }
    }

    #[test]
    fn unattended_needs_schema_4_a_reading_the_agent_at_boot_not_will_and_the_same_filesystem() {
        let target = mirror_target("system-recovery-A-2tb", Some(UUID_A));
        let good = stored_drive(agent_at_boot(), BootVerdict::May, Some(uuid_a()));
        let state = state_v(4, &[("system-recovery-A-2tb", good.clone())]);
        assert_eq!(
            unattended_possible(&target, &state, state.drives.get(&target.label)),
            Ok(())
        );

        let v3 = state_v(3, &[("system-recovery-A-2tb", good.clone())]);
        assert!(
            unattended_possible(&target, &v3, v3.drives.get(&target.label))
                .unwrap_err()
                .contains("schema 4")
        );
        assert!(
            unattended_possible(&target, &state, None)
                .unwrap_err()
                .contains("no record")
        );

        let mut err = good.clone();
        err.os = None;
        err.error = Some("could not read".into());
        assert!(
            unattended_possible(&target, &state, Some(&err))
                .unwrap_err()
                .contains("could not read")
        );

        let no_agent = stored_drive(
            agent_not_installed(),
            BootVerdict::May,
            good.mount_uuid.clone(),
        );
        assert!(
            unattended_possible(&target, &state, Some(&no_agent))
                .unwrap_err()
                .contains("guest agent")
        );

        let will = stored_drive(agent_at_boot(), BootVerdict::Will, good.mount_uuid.clone());
        assert!(
            unattended_possible(&target, &state, Some(&will))
                .unwrap_err()
                .contains("will run btrbk")
        );

        let other_fs = stored_drive(
            agent_at_boot(),
            BootVerdict::May,
            Some("7c7ae72d-09d6-4086-b249-1ac60f21b73b".into()),
        );
        assert!(
            unattended_possible(&target, &state, Some(&other_fs))
                .unwrap_err()
                .contains("filesystem")
        );
        let no_uuid = stored_drive(agent_at_boot(), BootVerdict::May, None);
        assert!(
            unattended_possible(&target, &state, Some(&no_uuid))
                .unwrap_err()
                .contains("filesystem")
        );
        let target_no_uuid = mirror_target("system-recovery-A-2tb", None);
        assert!(
            unattended_possible(&target_no_uuid, &state, Some(&good))
                .unwrap_err()
                .contains("mount_uuid")
        );
    }

    #[test]
    fn parallel_is_the_default_only_once_both_drives_have_three_clean_runs() {
        assert_eq!(pair_mode_default(&[Some(3), Some(3)]), "parallel");
        assert_eq!(pair_mode_default(&[Some(7), Some(3)]), "parallel");
        assert_eq!(pair_mode_default(&[Some(3), Some(2)]), "sequential");
        assert_eq!(pair_mode_default(&[None, Some(9)]), "sequential");
        assert_eq!(pair_mode_default(&[]), "sequential");
        assert_eq!(pair_mode_default(&[Some(3)]), "sequential");
    }

    #[test]
    fn due_is_stale_by_age_not_by_any_other_reason() {
        let by_age = Assessment {
            age_days: Some(61),
            stale: true,
            reasons: vec!["last applied full upgrade 61 days ago".into()],
            ..Default::default()
        };
        assert!(due(&by_age, 60));
        let by_kernel = Assessment {
            age_days: Some(3),
            stale: true,
            reasons: vec!["kernel series behind".into()],
            ..Default::default()
        };
        assert!(!due(&by_kernel, 60));
        let unknown = Assessment {
            age_days: None,
            stale: true,
            ..Default::default()
        };
        assert!(
            !due(&unknown, 60),
            "an unknown age is not 'due' (it is reported stale, not scheduled)"
        );
        assert!(due(
            &Assessment {
                age_days: Some(60),
                stale: true,
                ..Default::default()
            },
            60
        ));
    }

    #[test]
    fn status_json_has_every_key_the_gui_reads_and_only_mirror_targets() {
        let cfg = two_mirrors_and_a_primary();
        let state = Ok(Some(state_v(
            4,
            &[(
                "system-recovery-A-2tb",
                stored_drive(agent_at_boot(), BootVerdict::May, Some(uuid_a())),
            )],
        )));
        let reads = Scripted {
            clean: [
                ("system-recovery-A-2tb", Ok(2)),
                (
                    "system-recovery-B-2tb",
                    Err("history cannot be read".into()),
                ),
            ]
            .into(),
            history: [(
                "system-recovery-A-2tb",
                Ok(vec![
                    json!({"label":"system-recovery-A-2tb","outcome":"clean"}),
                ]),
            )]
            .into(),
            ..Default::default()
        };
        let j = status_json(&cfg, &state, &host(), "2026-10-08", 1_791_500_000, &reads);
        assert_eq!(j["schema"], 1);
        assert_eq!(j["max_age_days"], 60);
        assert_eq!(j["today"], "2026-10-08");
        assert!(
            j["drives"][0]["session_error"].is_null(),
            "a reading, not an error"
        );
        for key in [
            "mode_default",
            "schedule",
            "schedule_error",
            "session",
            "session_error",
        ] {
            assert!(j["pair"].get(key).is_some(), "pair missing {key}");
        }
        assert_eq!(
            j["drives"].as_array().unwrap().len(),
            2,
            "primary-22tb is not a drive here"
        );
        let a = &j["drives"][0];
        for key in [
            "label",
            "display_name",
            "serials",
            "checked_epoch",
            "record_error",
            "record",
            "assessment",
            "due",
            "verdict",
            "unattended",
            "clean_runs",
            "clean_runs_error",
            "history",
            "history_error",
            "schedule",
            "schedule_error",
            "session",
            "session_error",
        ] {
            assert!(a.get(key).is_some(), "missing {key}: {a}");
        }
        for key in [
            "os",
            "installed",
            "last_full_upgrade",
            "kernel",
            "host_kernel",
            "btrfs_progs",
            "host_btrfs_progs",
            "btrbk",
            "guest_agent",
        ] {
            assert!(a["record"].get(key).is_some(), "record missing {key}");
        }
        assert_eq!(a["record"]["kernel"], "6.17.1-1-cachyos");
        assert_eq!(a["verdict"], "may");
        assert_eq!(a["unattended"]["possible"], true);
        assert_eq!(a["unattended"]["why"], "");
        assert_eq!(a["clean_runs"], 2);
        assert!(a["clean_runs_error"].is_null());
        assert_eq!(a["history"].as_array().unwrap().len(), 1);
        assert!(a["history_error"].is_null());
        let b = &j["drives"][1];
        assert!(
            b["record"].is_null() && b["checked_epoch"].is_null(),
            "B has no reading"
        );
        assert!(b["verdict"].is_null() && b["assessment"].is_null());
        assert_eq!(b["unattended"]["possible"], false);
        assert!(!b["unattended"]["why"].as_str().unwrap().is_empty());
        assert!(
            b["clean_runs"].is_null(),
            "a refused count is null, never 0"
        );
        assert_eq!(b["clean_runs_error"], "history cannot be read");
        assert!(b["record_error"].as_str().unwrap().contains("no record"));
        assert_eq!(j["pair"]["mode_default"], "sequential");
    }

    #[test]
    fn status_json_with_an_unreadable_state_file_says_so_on_every_drive() {
        let cfg = two_mirrors_and_a_primary();
        let state: Result<Option<StoredState>, String> =
            Err("/var/lib/das-backup/recovery-os.json: corrupt".into());
        let j = status_json(
            &cfg,
            &state,
            &host(),
            "2026-10-08",
            1_791_500_000,
            &Scripted::default(),
        );
        for d in j["drives"].as_array().unwrap() {
            assert!(d["record_error"].as_str().unwrap().contains("corrupt"));
            assert_eq!(d["unattended"]["possible"], false);
        }
    }

    #[test]
    fn history_shows_only_the_last_twenty_and_parallel_needs_both_counts() {
        let cfg = two_mirrors_and_a_primary();
        let lines: Vec<Value> = (0..25).map(|i| json!({"n": i})).collect();
        let reads = Scripted {
            clean: [
                ("system-recovery-A-2tb", Ok(3)),
                ("system-recovery-B-2tb", Ok(3)),
            ]
            .into(),
            history: [("system-recovery-A-2tb", Ok(lines))].into(),
            ..Default::default()
        };
        let j = status_json(
            &cfg,
            &Ok(None),
            &host(),
            "2026-10-08",
            1_791_500_000,
            &reads,
        );
        let h = j["drives"][0]["history"].as_array().unwrap();
        assert_eq!(h.len(), 20);
        assert_eq!(h[0]["n"], 5);
        assert_eq!(h[19]["n"], 24, "newest last");
        assert_eq!(j["pair"]["mode_default"], "parallel");
    }

    #[test]
    fn an_unreadable_history_is_an_error_not_an_empty_list() {
        let cfg = two_mirrors_and_a_primary();
        let reads = Scripted {
            history: [("system-recovery-A-2tb", Err("history is corrupt".into()))].into(),
            ..Default::default()
        };
        let j = status_json(
            &cfg,
            &Ok(None),
            &host(),
            "2026-10-08",
            1_791_500_000,
            &reads,
        );
        let a = &j["drives"][0];
        assert_eq!(a["history_error"], "history is corrupt");
        assert!(a["history"].as_array().unwrap().is_empty());
        // B has no scripted error: an empty history is a reading, no error.
        assert!(j["drives"][1]["history_error"].is_null());
    }

    #[test]
    fn due_reaches_the_document_only_for_a_drive_stale_by_age() {
        let cfg = two_mirrors_and_a_primary();
        let state = Ok(Some(state_v(
            4,
            &[(
                "system-recovery-A-2tb",
                stored_drive(agent_at_boot(), BootVerdict::May, Some(uuid_a())),
            )],
        )));
        // Last full upgrade 2026-09-20: 18 days old on 2026-10-08, 103 on 2027-01-01.
        let fresh = status_json(
            &cfg,
            &state,
            &host(),
            "2026-10-08",
            1_791_500_000,
            &Scripted::default(),
        );
        assert_eq!(fresh["max_age_days"], 60);
        assert_eq!(fresh["drives"][0]["due"], false);
        let old = status_json(
            &cfg,
            &state,
            &host(),
            "2027-01-01",
            1_791_500_000,
            &Scripted::default(),
        );
        assert_eq!(old["max_age_days"], cfg.recovery_os.max_age_days);
        assert!(old["drives"][0]["assessment"]["age_days"].as_i64().unwrap() >= 60);
        assert_eq!(old["drives"][0]["due"], true);
        // B has no reading: nothing to be due.
        assert_eq!(old["drives"][1]["due"], false);
    }

    // --- schedules (Task 6)

    #[test]
    fn the_service_unit_runs_the_script_unattended_waits_for_the_lock_and_never_lands_failed() {
        let s = render_service(
            &["system-recovery-A-2tb".into()],
            None,
            Path::new("/usr/lib/das-backup/recovery-os-vm.sh"),
        );
        assert!(s.contains("ExecStart=/usr/lib/das-backup/recovery-os-vm.sh session system-recovery-A-2tb --unattended --wait-lock 180\n"), "{s}");
        assert!(
            s.contains("\nType=oneshot\n") && s.contains("\nSuccessExitStatus=1 3 4 5 6 7\n"),
            "{s}"
        );
        assert!(
            !s.contains("Restart="),
            "sentinel must never see this unit failed, and systemd must never restart it"
        );
        assert!(
            s.contains("After=network-online.target libvirtd.service\n")
                && s.contains("Wants=network-online.target\n")
        );
        let both = render_service(
            &[
                "system-recovery-A-2tb".into(),
                "system-recovery-B-2tb".into(),
            ],
            Some("parallel"),
            Path::new("/x.sh"),
        );
        assert!(both.contains("ExecStart=/x.sh session system-recovery-A-2tb system-recovery-B-2tb --mode parallel --unattended --wait-lock 180\n"));
    }

    #[test]
    fn the_timer_is_one_shot_local_time_and_not_persistent() {
        let t = render_timer(&["system-recovery-A-2tb".into()], 1791500000); // 2026-10-08 17:53:20 CDT
        // The brief pinned "2026-10-08 17:53:20", true only in America/Chicago;
        // the system `date` is the oracle for the zone the test runs in.
        let out = std::process::Command::new("date")
            .args(["-d", "@1791500000", "+%F %T"])
            .output()
            .unwrap();
        let local = String::from_utf8(out.stdout).unwrap().trim().to_string();
        assert!(t.contains(&format!("OnCalendar={local}\n")), "{t}");
        assert!(
            t.contains("Persistent=false\n")
                && t.contains("Unit=das-recovery-os-update-system-recovery-A-2tb.service\n")
                && t.contains("WantedBy=timers.target\n")
        );
    }

    #[test]
    fn a_timer_whose_time_passed_without_a_trigger_reads_missed() {
        let timer = UnitFacts {
            exists: true,
            active_state: "active".into(),
            next_elapse_epoch: None,
            last_trigger_epoch: None,
            on_calendar: Some("2026-10-08 03:00:00".into()),
            ..Default::default()
        };
        let service = UnitFacts {
            exists: true,
            active_state: "inactive".into(),
            ..Default::default()
        };
        let s = schedule_state(&timer, &service, None, 1791500000);
        assert_eq!(s.state, "missed");
        assert!(s.detail.contains("2026-10-08 03:00:00"));
    }

    #[test]
    fn a_fired_schedule_with_no_history_line_reads_the_units_result() {
        let timer = UnitFacts {
            exists: true,
            active_state: "active".into(),
            last_trigger_epoch: Some(1791490000),
            ..Default::default()
        };
        let service = UnitFacts {
            exists: true,
            active_state: "inactive".into(),
            result: Some("success".into()),
            exec_main_status: Some(1),
            last_journal: vec!["refuse: the boot record says btrbk will run".into()],
            ..Default::default()
        };
        let s = schedule_state(&timer, &service, None, 1791500000);
        assert_eq!(s.state, "fired");
        assert!(s.detail.starts_with("refused:"), "{}", s.detail);
        assert!(s.detail.contains("will run"));
        let newer = serde_json::json!({"start": 1791490100, "outcome": "clean", "exit": 0});
        let s = schedule_state(&timer, &service, Some(&newer), 1791500000);
        assert_eq!(s.detail, "clean (exit 0)");
        let older = serde_json::json!({"start": 1791000000, "outcome": "clean", "exit": 0});
        assert!(
            schedule_state(&timer, &service, Some(&older), 1791500000)
                .detail
                .starts_with("refused:"),
            "an older history line is not this firing's"
        );
        let running = UnitFacts {
            exists: true,
            active_state: "activating".into(),
            exec_main_start_epoch: Some(1791499000),
            ..Default::default()
        };
        assert_eq!(
            schedule_state(&timer, &running, None, 1791500000).state,
            "running"
        );
        let pending = UnitFacts {
            exists: true,
            active_state: "active".into(),
            next_elapse_epoch: Some(1791600000),
            ..Default::default()
        };
        assert_eq!(
            schedule_state(&pending, &service, None, 1791500000).state,
            "pending"
        );
    }

    #[test]
    fn a_timer_set_again_after_it_fired_reads_pending_and_its_mode_comes_from_the_service() {
        // LastTriggerUSec outlives a re-schedule of the same unit.
        let timer = UnitFacts {
            exists: true,
            active_state: "active".into(),
            last_trigger_epoch: Some(1791490000),
            next_elapse_epoch: Some(1791600000),
            on_calendar: Some("2026-10-08 03:00:00".into()),
            ..Default::default()
        };
        let service = UnitFacts {
            exists: true,
            active_state: "inactive".into(),
            exec_start: Some(
                "/x.sh session a b --mode parallel --unattended --wait-lock 180".into(),
            ),
            ..Default::default()
        };
        let s = schedule_state(&timer, &service, None, 1791500000);
        assert_eq!(s.state, "pending");
        assert_eq!(s.mode.as_deref(), Some("parallel"));
        assert_eq!(
            s.at_epoch,
            crate::caldate::local_epoch("2026-10-08 03:00:00")
        );
        // A time that cannot be read is null, never 0.
        let unread = UnitFacts {
            on_calendar: None,
            ..timer
        };
        assert_eq!(
            schedule_state(&unread, &service, None, 1791500000).at_epoch,
            None
        );
    }

    #[test]
    fn validate_schedule_refuses_the_past_a_will_record_a_mode_for_one_drive_and_a_drive_already_scheduled_the_other_way()
     {
        let cfg = two_mirrors_and_a_primary();
        let b_uuid = "7c7ae72d-09d6-4086-b249-1ac60f21b73b".to_string();
        let state = state_v(
            4,
            &[
                (
                    "system-recovery-A-2tb",
                    stored_drive(agent_at_boot(), BootVerdict::May, Some(uuid_a())),
                ),
                (
                    "system-recovery-B-2tb",
                    stored_drive(agent_at_boot(), BootVerdict::Will, Some(b_uuid)),
                ),
            ],
        );
        let now = 1_791_500_000;
        let a = vec!["system-recovery-A-2tb".to_string()];
        let ab = vec![
            "system-recovery-A-2tb".to_string(),
            "system-recovery-B-2tb".to_string(),
        ];
        assert_eq!(
            validate_schedule(&cfg, &state, &a, now + 120, None, now, &[]),
            Ok(())
        );
        assert!(
            validate_schedule(&cfg, &state, &a, now + 119, None, now, &[])
                .unwrap_err()
                .contains("120")
        );
        assert!(
            validate_schedule(&cfg, &state, &a, now - 3600, None, now, &[])
                .unwrap_err()
                .contains("future")
        );
        assert!(
            validate_schedule(&cfg, &state, &ab, now + 600, Some("parallel"), now, &[])
                .unwrap_err()
                .contains("will run btrbk")
        );
        assert!(
            validate_schedule(&cfg, &state, &a, now + 600, Some("parallel"), now, &[])
                .unwrap_err()
                .contains("two drives")
        );
        assert!(
            validate_schedule(
                &cfg,
                &state,
                &["primary-22tb".to_string()],
                now + 600,
                None,
                now,
                &[]
            )
            .unwrap_err()
            .contains("not a recovery drive")
        );
        let both = vec![format!("{UNIT_PREFIX}both")];
        assert!(
            validate_schedule(&cfg, &state, &a, now + 600, None, now, &both)
                .unwrap_err()
                .contains("clear it first")
        );
        let per_drive = vec![format!("{UNIT_PREFIX}system-recovery-A-2tb")];
        let mut fixed = state.clone();
        fixed
            .drives
            .get_mut("system-recovery-B-2tb")
            .unwrap()
            .os
            .as_mut()
            .unwrap()
            .btrbk_at_boot
            .verdict = BootVerdict::May;
        assert!(
            validate_schedule(&cfg, &fixed, &ab, now + 600, None, now, &per_drive)
                .unwrap_err()
                .contains("clear it first")
        );
        assert_eq!(
            validate_schedule(
                &cfg,
                &fixed,
                &ab,
                now + 600,
                None,
                now,
                &[format!("{UNIT_PREFIX}both")]
            ),
            Ok(()),
            "the same kind is replaced, not refused"
        );
        assert_eq!(
            unit_base(&a),
            "das-recovery-os-update-system-recovery-A-2tb"
        );
        assert_eq!(unit_base(&ab), "das-recovery-os-update-both");
    }

    #[test]
    fn the_guest_agent_is_put_in_the_exact_words_the_gui_shows() {
        assert_eq!(agent_words(&agent_at_boot()), "installed, enabled");
        assert_eq!(
            agent_words(&GuestAgent::Read {
                installed: true,
                enabled: false,
                why: "x".into()
            }),
            "installed, not enabled"
        );
        assert_eq!(agent_words(&agent_not_installed()), "not installed");
        assert_eq!(
            agent_words(&GuestAgent::Unreadable {
                reason: "no log".into()
            }),
            "unreadable: no log"
        );
        // And the sentence a refusal carries.
        let target = mirror_target("system-recovery-A-2tb", Some(UUID_A));
        let no_agent = stored_drive(agent_not_installed(), BootVerdict::May, Some(uuid_a()));
        let state = state_v(4, &[("system-recovery-A-2tb", no_agent.clone())]);
        assert!(
            unattended_possible(&target, &state, Some(&no_agent))
                .unwrap_err()
                .contains("in system-recovery-A-2tb's OS (not installed);")
        );
    }

    #[test]
    fn a_pending_timer_is_due_now_exactly_when_its_next_elapse_is_not_after_now() {
        let now = 1_791_500_000;
        let service = UnitFacts {
            exists: true,
            active_state: "inactive".into(),
            ..Default::default()
        };
        let pending = |next: i64| {
            let timer = UnitFacts {
                exists: true,
                active_state: "active".into(),
                next_elapse_epoch: Some(next),
                ..Default::default()
            };
            schedule_state(&timer, &service, None, now)
        };
        let at = |epoch: i64| crate::caldate::local_datetime(epoch).unwrap();
        // The sentence is pinned whole, local time included.
        assert_eq!(
            pending(now + 1).detail,
            format!("The session will run unattended at {}.", at(now + 1))
        );
        assert_eq!(
            pending(now).detail,
            format!("The session will run unattended at {} (due now).", at(now))
        );
        assert_eq!(
            pending(now - 1).detail,
            format!(
                "The session will run unattended at {} (due now).",
                at(now - 1)
            )
        );
        assert_eq!(pending(now).state, "pending");
    }

    #[test]
    fn local_times_in_sentences_are_the_hosts_wall_clock() {
        let service_running = UnitFacts {
            exists: true,
            active_state: "active".into(),
            exec_main_start_epoch: Some(1_791_499_000),
            ..Default::default()
        };
        let timer = UnitFacts::default();
        let s = schedule_state(&timer, &service_running, None, 1_791_500_000);
        assert_eq!(
            s.detail,
            format!(
                "The scheduled session is running since {}.",
                crate::caldate::local_datetime(1_791_499_000).unwrap()
            )
        );
        let fired = UnitFacts {
            exists: true,
            active_state: "active".into(),
            last_trigger_epoch: Some(1_791_490_000),
            ..Default::default()
        };
        let idle = UnitFacts {
            exists: true,
            active_state: "inactive".into(),
            ..Default::default()
        };
        let s = schedule_state(&fired, &idle, None, 1_791_500_000);
        assert!(
            s.detail.contains(&format!(
                "(fired at {};",
                crate::caldate::local_datetime(1_791_490_000).unwrap()
            )),
            "{}",
            s.detail
        );
        // No local time to give: the epoch itself.
        assert_eq!(local_words(i64::MAX), format!("epoch {}", i64::MAX));
    }

    #[test]
    fn a_timer_that_never_fired_says_why_by_its_own_state() {
        let idle = UnitFacts {
            exists: true,
            active_state: "inactive".into(),
            ..Default::default()
        };
        let with = |state: &str| {
            let timer = UnitFacts {
                exists: true,
                active_state: state.into(),
                on_calendar: Some("2026-10-08 03:00:00".into()),
                ..Default::default()
            };
            let s = schedule_state(&timer, &idle, None, 1_791_500_000);
            assert_eq!(s.state, "missed");
            s.detail
        };
        assert_eq!(
            with("active"),
            "The time 2026-10-08 03:00:00 passed without the timer firing (the host was off, or asleep); it will not run until it is scheduled again."
        );
        assert_eq!(
            with("inactive"),
            "The timer for 2026-10-08 03:00:00 is inactive and never fired; it will not run until it is scheduled again."
        );
        assert_eq!(
            with(""),
            "The timer for 2026-10-08 03:00:00 is in a state systemctl did not name and never fired; it will not run until it is scheduled again."
        );
    }

    #[test]
    fn a_label_that_cannot_name_a_unit_is_refused_on_either_count_alone() {
        let mut cfg = two_mirrors_and_a_primary();
        // Both are configured mirrors, so the request itself is valid; only
        // the unit-name rule can refuse them.
        cfg.targets.push(mirror_target("both", Some(UUID_A)));
        cfg.targets.push(mirror_target("has space", Some(UUID_A)));
        let state = state_v(4, &[]);
        let now = 1_791_500_000;
        for label in ["both", "has space"] {
            let err = validate_schedule(
                &cfg,
                &state,
                &[label.to_string()],
                now + 600,
                None,
                now,
                &[],
            )
            .unwrap_err();
            assert!(
                err.contains("cannot name a schedule unit"),
                "{label}: {err}"
            );
        }
    }

    #[test]
    fn the_first_running_schedule_service_is_the_one_named() {
        let a = "system-recovery-A-2tb";
        let own = format!("{UNIT_PREFIX}{a}.service");
        let pair = format!("{UNIT_PREFIX}both.service");
        let running = |state: &str| {
            Ok(UnitFacts {
                exists: true,
                active_state: state.into(),
                exec_main_start_epoch: Some(1_791_499_000),
                ..Default::default()
            })
        };
        let mut reads = Scripted::default();
        reads.units.insert(own.clone(), running("active"));
        reads.units.insert(pair.clone(), running("activating"));
        let (s, err) = session_of(a, None, None, &reads);
        assert_eq!(err, None);
        assert_eq!(s.unwrap().by, format!("unit:{own}"));
        // Only the pair's running: that one.
        let mut reads = Scripted::default();
        reads.units.insert(pair.clone(), running("active"));
        let (s, _) = session_of(a, None, None, &reads);
        assert_eq!(s.unwrap().by, format!("unit:{pair}"));
    }

    #[test]
    fn status_fills_schedules_and_sessions_from_units_jobs_and_the_lock() {
        let cfg = two_mirrors_and_a_primary();
        let a_base = format!("{UNIT_PREFIX}system-recovery-A-2tb");
        let both = format!("{UNIT_PREFIX}both");
        let mut units = HashMap::new();
        units.insert(
            format!("{a_base}.timer"),
            Ok(UnitFacts {
                exists: true,
                active_state: "active".into(),
                next_elapse_epoch: Some(1_791_600_000),
                on_calendar: Some("2026-10-09 22:00:00".into()),
                ..Default::default()
            }),
        );
        units.insert(
            format!("{both}.timer"),
            Err("systemctl cannot be asked".to_string()),
        );
        let reads = Scripted {
            units,
            lock: Some("recovery-os VM session system-recovery-B-2tb pid 4242".into()),
            domain: Some("running".into()),
            ..Default::default()
        };
        let j = status_json(
            &cfg,
            &Ok(None),
            &host(),
            "2026-10-08",
            1_791_500_000,
            &reads,
        );
        let a = &j["drives"][0];
        assert_eq!(a["schedule"]["state"], "pending", "{a}");
        assert_eq!(a["schedule"]["unit"], format!("{a_base}.timer"));
        assert!(a["schedule_error"].is_null());
        assert!(a["session"].is_null(), "the lock names B, not A");
        let b = &j["drives"][1];
        assert!(
            b["schedule"].is_null() && b["schedule_error"].is_null(),
            "B has no unit: no schedule, and that is a reading"
        );
        assert_eq!(
            b["session"]["by"],
            "other:recovery-os VM session system-recovery-B-2tb pid 4242"
        );
        assert!(b["session"]["since_epoch"].is_null() && b["session"]["attended"].is_null());
        assert_eq!(b["session"]["domain_state"], "running");
        assert!(j["pair"]["schedule"].is_null());
        assert!(
            j["pair"]["schedule_error"]
                .as_str()
                .unwrap()
                .contains("cannot be asked"),
            "an unreadable unit is an error, never 'no schedule'"
        );

        let job = Scripted {
            job: Some("job-7".into()),
            lock: Some("recovery-os VM session system-recovery-B-2tb pid 4242".into()),
            ..Default::default()
        };
        let j = status_json(&cfg, &Ok(None), &host(), "2026-10-08", 1_791_500_000, &job);
        assert_eq!(j["drives"][1]["session"]["by"], "job:job-7");

        let mut units = HashMap::new();
        units.insert(
            format!("{both}.service"),
            Ok(UnitFacts {
                exists: true,
                active_state: "activating".into(),
                exec_main_start_epoch: Some(1_791_499_000),
                ..Default::default()
            }),
        );
        let unit = Scripted {
            units,
            ..Default::default()
        };
        let j = status_json(&cfg, &Ok(None), &host(), "2026-10-08", 1_791_500_000, &unit);
        for d in j["drives"].as_array().unwrap() {
            assert_eq!(d["session"]["by"], format!("unit:{both}.service"));
            assert_eq!(d["session"]["since_epoch"], 1_791_499_000);
            assert_eq!(d["session"]["attended"], false);
        }
        assert_eq!(j["pair"]["session"]["by"], format!("unit:{both}.service"));
    }

    #[test]
    fn an_unreadable_schedule_service_is_a_session_error_never_no_session() {
        let cfg = two_mirrors_and_a_primary();
        let both = format!("{UNIT_PREFIX}both.service");
        let mut units = HashMap::new();
        units.insert(
            format!("{UNIT_PREFIX}system-recovery-A-2tb.service"),
            Err("systemctl show failed (scripted)".to_string()),
        );
        units.insert(both.clone(), Err("bus unreachable (scripted)".to_string()));
        let reads = Scripted {
            units,
            ..Default::default()
        };
        let j = status_json(
            &cfg,
            &Ok(None),
            &host(),
            "2026-10-08",
            1_791_500_000,
            &reads,
        );
        let a = &j["drives"][0];
        assert!(a["session"].is_null());
        let e = a["session_error"].as_str().unwrap_or_else(|| panic!("{a}"));
        assert!(e.contains("show failed (scripted)"), "{e}");
        let b = &j["drives"][1];
        assert!(
            b["session_error"]
                .as_str()
                .unwrap()
                .contains("bus unreachable"),
            "B is covered by the pair's service: {b}"
        );
        assert!(j["pair"]["session"].is_null());
        assert!(
            j["pair"]["session_error"]
                .as_str()
                .unwrap()
                .contains("bus unreachable")
        );
    }
}
