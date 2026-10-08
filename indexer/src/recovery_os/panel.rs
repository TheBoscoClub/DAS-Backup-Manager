//! The recovery-drive panel's rules and its status document (bd 8249 stage 2).
//! Pure: everything the system says comes through [`PanelReads`].
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
    /// The last 5 journal lines.
    pub last_journal: Vec<String>,
}

/// A scheduled session (filled in by the schedule layer).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Schedule {
    pub unit: String,
    pub at_epoch: i64,
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
    pub session: Option<Session>,
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
            "session": self.session,
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

fn drive_status(
    cfg: &Config,
    target: &Target,
    state: &Result<Option<StoredState>, String>,
    host: &HostVersions,
    today: &str,
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
        schedule: None,
        session: None,
    }
}

/// The document the GUI renders: every `role = "mirror"` target in config
/// order, each with its record, verdict, reasons and history.
pub fn status_json(
    cfg: &Config,
    state: &Result<Option<StoredState>, String>,
    host: &HostVersions,
    today: &str,
    reads: &dyn PanelReads,
) -> Value {
    let drives: Vec<DriveStatus> = cfg
        .targets
        .iter()
        .filter(|t| t.role == TargetRole::Mirror)
        .map(|t| drive_status(cfg, t, state, host, today, reads))
        .collect();
    let mode_default = if drives.len() == 2 {
        pair_mode_default(&[drives[0].clean_runs, drives[1].clean_runs])
    } else {
        "sequential"
    };
    json!({
        "schema": 1,
        "max_age_days": cfg.recovery_os.max_age_days,
        "today": today,
        "pair": {"mode_default": mode_default, "schedule": null, "session": null},
        "drives": drives.iter().map(DriveStatus::to_json).collect::<Vec<_>>(),
    })
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
        fn unit(&self, _name: &str) -> Result<UnitFacts, String> {
            Ok(UnitFacts::default())
        }
        fn domain_state(&self, _label: &str) -> Option<String> {
            None
        }
        fn lock_holder(&self) -> Option<String> {
            None
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
        };
        let j = status_json(&cfg, &state, &host(), "2026-10-08", &reads);
        assert_eq!(j["schema"], 1);
        assert_eq!(j["max_age_days"], 60);
        assert_eq!(j["today"], "2026-10-08");
        for key in ["mode_default", "schedule", "session"] {
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
            "session",
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
        let j = status_json(&cfg, &state, &host(), "2026-10-08", &Scripted::default());
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
        };
        let j = status_json(&cfg, &Ok(None), &host(), "2026-10-08", &reads);
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
        let j = status_json(&cfg, &Ok(None), &host(), "2026-10-08", &reads);
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
        let fresh = status_json(&cfg, &state, &host(), "2026-10-08", &Scripted::default());
        assert_eq!(fresh["max_age_days"], 60);
        assert_eq!(fresh["drives"][0]["due"], false);
        let old = status_json(&cfg, &state, &host(), "2027-01-01", &Scripted::default());
        assert_eq!(old["max_age_days"], cfg.recovery_os.max_age_days);
        assert!(old["drives"][0]["assessment"]["age_days"].as_i64().unwrap() >= 60);
        assert_eq!(old["drives"][0]["due"], true);
        // B has no reading: nothing to be due.
        assert_eq!(old["drives"][1]["due"], false);
    }
}
