// Template engine — renders config files and scripts from a Config struct.
// Each function builds output strings programmatically (no external template files).

#![allow(dead_code)]

use super::config::*;

pub use buttered_dasd::btrbk_conf::{GENERATED_HEADER, render_btrbk_conf, render_state_conf};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// Production scripts embedded in binary (installed to ${prefix}/lib/das-backup/, flat)
const SCRIPT_BACKUP_RUN: &str = include_str!("../../../scripts/backup-run.sh");
const SCRIPT_BACKUP_VERIFY: &str = include_str!("../../../scripts/backup-verify.sh");
const SCRIPT_BOOT_ARCHIVE_CLEANUP: &str = include_str!("../../../scripts/boot-archive-cleanup.sh");

/// Basenames of the files above — the only generated files whose content is
/// fixed at build time rather than derived from `Config`.
///
/// This distinction is what the installer's staleness guard
/// (bd `DAS-Backup-Manager-2lj`) actually rests on. A binary built before a
/// script edit carries a stale `include_str!` copy, so refusing to overwrite an
/// on-disk script newer than the binary is correct. Every OTHER generated file —
/// `btrbk.conf`, every systemd unit, the cron entry — is rendered from the
/// user's `config.toml` by `render_*`, so its content depends on the config and
/// never on the binary's vintage. Guarding those by binary mtime made config
/// edits inert: a successful upgrade stamps them with `now`, which outlives the
/// binary, so the next upgrade refused to rewrite them
/// (bd `DAS-Backup-Manager-bwt`).
const EMBEDDED_SCRIPTS: [&str; 3] = [
    "backup-run.sh",
    "backup-verify.sh",
    "boot-archive-cleanup.sh",
];

/// True when `path` names one of the [`EMBEDDED_SCRIPTS`], i.e. a file whose
/// content came from `include_str!` and may legitimately be older in the binary
/// than on disk. Matches on the final path component so it holds regardless of
/// the configured `install_prefix`.
pub fn is_embedded_script(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    EMBEDDED_SCRIPTS.contains(&base)
}

// ---------------------------------------------------------------------------
// Render functions
// ---------------------------------------------------------------------------

/// Generate a systemd .service unit file. When `full` is true, appends " --full"
/// to the ExecStart line.
///
/// `TimeoutStartSec=infinity` (not a fixed bound): `backup-run.sh` takes the
/// blocking `/run/das-maintenance.lock` shared with the scrub engine
/// (`indexer/src/scrub.rs`, bd DAS-Backup-Manager-212/atq). A scrub pass gives
/// that lock up to a backup that claims it (bd DAS-Backup-Manager-su1b.2), so
/// the wait behind a scrub is about a minute; but a recovery-OS VM session or
/// an older scrub binary does not answer a claim, and a backup then still
/// waits — potentially for hours. A finite `TimeoutStartSec` would SIGKILL the
/// backup mid-wait. The maintenance lock is the correctness mechanism against
/// overlap, not a timeout (user decision 2026-07-27, bd DAS-Backup-Manager-b6f).
///
/// Ordered after `time-sync.target`: the run dates retirements and expires
/// retired backups by the clock. This ordering is a courtesy, not the guard —
/// `caldate::untrusted_clock` refuses a date before 2026 whatever the order.
///
/// `SuccessExitStatus=3` (bd DAS-Backup-Manager-d1r, operator decision C):
/// `backup-run.sh` exits 3 when the run began its work and something failed
/// — btrbk failed for some or all targets, an operation failed, or it aborted
/// on a target's or a source's state. What caused it is usually still there
/// ten minutes later, and with the unit `failed` cachyos-sentinel would start
/// a whole new backup about every ten minutes — one absent drive was enough
/// (measured 2026-10-02 by hand: btrbk exited 10, the script 1). Exit 1,
/// could not start, still fails the unit.
///
/// Setup is the only writer of this unit and its timer. `cmake --install`
/// shipped its own copies until bd DAS-Backup-Manager-7rf; `setup --upgrade`
/// removes the ones older versions left (`super::retired_units`).
pub fn render_systemd_service(config: &Config, full: bool) -> String {
    let script_dir = format!("{}/lib/das-backup", config.general.install_prefix);
    let desc = if full {
        "DAS Backup - Full BTRFS backup"
    } else {
        "DAS Backup - Incremental BTRFS backup"
    };
    let exec_args = if full { " --full" } else { "" };

    format!(
        "{GENERATED_HEADER}\n\
         [Unit]\n\
         Description={desc}\n\
         After=local-fs.target\n\
         Wants=time-sync.target\n\
         After=time-sync.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         # Exit semantics (bd DAS-Backup-Manager-d1r):\n\
         #   0 = the run executed and nothing failed (a warning still exits 0), or it\n\
         #       skipped because another backup was running.\n\
         #   3 = the run began its work and something failed: btrbk failed for some or\n\
         #       all targets, any operation failed, or it aborted on a target's or a\n\
         #       source's state. Its report says what (an abort sends a short ABORTED\n\
         #       report), the history has it as failed, and the journal has status=3.\n\
         #   1 = could not start: nothing was mounted or sent (config, btrdasd missing,\n\
         #       arguments, not root, a lock it could not open or take).\n\
         #   129 130 138 141 142 143 = stopped by a signal (HUP INT USR1 PIPE ALRM TERM).\n\
         # SuccessExitStatus=3: what made a run fail is usually still there ten minutes\n\
         # later (an absent drive). With the unit failed, cachyos-sentinel would start\n\
         # a whole new backup about every ten minutes; its 3-per-600 s limiter cannot\n\
         # brake a loop that slow. Exit 1 still fails the unit, in seconds.\n\
         SuccessExitStatus=3\n\
         ExecStart={script_dir}/backup-run.sh{exec_args}\n\
         StandardOutput=journal\n\
         StandardError=journal\n\
         Nice=19\n\
         IOSchedulingClass=idle\n\
         TimeoutStartSec=infinity\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Generate a systemd .timer unit file. Converts schedule strings to OnCalendar
/// format: "03:00" becomes "*-*-* 03:00:00", "Sun 04:00" becomes "Sun *-*-* 04:00:00".
pub fn render_systemd_timer(config: &Config, full: bool) -> String {
    let (desc, calendar) = if full {
        (
            "DAS Backup Timer - Weekly full backup",
            &config.schedule.full,
        )
    } else {
        (
            "DAS Backup Timer - Daily incremental backup",
            &config.schedule.incremental,
        )
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
        "{GENERATED_HEADER}\n\
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

/// Process priority the generated `das-scrub.service` carries (bd
/// DAS-Backup-Manager-su1b.1). `setup --check` reads the effective values back
/// from systemd and compares them with these, so a stale unit or a drop-in
/// that lowers the priority is reported.
pub const SCRUB_NICE: &str = "-20";
/// I/O scheduling class of the scrub unit (see [`SCRUB_NICE`]).
pub const SCRUB_IO_CLASS: &str = "best-effort";
/// I/O scheduling level of the scrub unit (see [`SCRUB_NICE`]).
pub const SCRUB_IO_LEVEL: &str = "0";

/// Generate the das-scrub.service unit file. Deliberately dumb by design
/// (user decision 2026-07-27, bd DAS-Backup-Manager-atq): NO `Conflicts=`
/// (which would stop the other unit — cancellation was explicitly scrapped
/// in favor of deferral), NO `ExecStopPost` cancel, NO `RuntimeMaxSec=`. The
/// scrub engine (`indexer/src/scrub.rs`) itself defers to a running backup
/// via `/run/das-maintenance.lock` shared with `backup-run.sh`, and yields
/// the lock to a backup that claims it mid-pass, resuming afterwards (bd
/// DAS-Backup-Manager-su1b.2), so a unit-layer mechanism would not cover
/// manual `btrdasd scrub run` invocations anyway. `TimeoutStartSec=infinity`
/// because the pass (and any lock wait or pause in it) is unbounded — a real
/// monthly pass across the DAS filesystems runs many hours.
///
/// `ExecStart` is preceded by a comment documenting the exit-code split
/// (`bd DAS-Backup-Manager-18p`): `btrdasd scrub run` exits 0 whenever the
/// pass ran, even if it found real damage, specifically so this unit never
/// lands in `failed` state for a finding that Sentinel's restart-rate
/// limiter cannot brake (failures 14–18 h apart never trip the 3/600s
/// window). See `exit_code_for_pass` in `indexer/src/main.rs`.
pub fn render_systemd_scrub_service(config: &Config) -> String {
    let prefix = &config.general.install_prefix;

    format!(
        "{GENERATED_HEADER}\n\
         [Unit]\n\
         Description=DAS Backup - Scheduled BTRFS scrub of backup targets\n\
         After=local-fs.target\n\
         Wants=time-sync.target\n\
         After=time-sync.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         # Exit semantics (bd DAS-Backup-Manager-18p): exit 0 means the pass\n\
         # RAN (findings are reported via email + health, not this exit code);\n\
         # nonzero means the pass could NOT run at all. This keeps a scrub\n\
         # that finds real damage from ever retry-looping under Sentinel.\n\
         # Priority (bd DAS-Backup-Manager-su1b.1, su1b.5): nice -20 and\n\
         # best-effort I/O level 0 here lift btrdasd and the btrfs main thread\n\
         # only. btrfs's per-device workers, which do the scrub's I/O, ignore\n\
         # this class and set their own from `btrfs scrub -c/-n`: btrdasd passes\n\
         # `-c 1 -n 0` (real-time) when every device of the filesystem runs\n\
         # mq-deadline with prio_aging_expire (starvation bounded), else\n\
         # `-c 2 -n 0` (best-effort), chosen per filesystem at each start.\n\
         ExecStart={prefix}/bin/btrdasd scrub run\n\
         StandardOutput=journal\n\
         StandardError=journal\n\
         Nice={SCRUB_NICE}\n\
         IOSchedulingClass={SCRUB_IO_CLASS}\n\
         IOSchedulingPriority={SCRUB_IO_LEVEL}\n\
         TimeoutStartSec=infinity\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Generate the das-scrub.timer unit file. `OnCalendar` is consumed verbatim
/// from `config.scrub.on_calendar` (default `*-*-01 03:05:00`, monthly) — see
/// the doc comment on `default_scrub_on_calendar()` in `indexer/src/config.rs`.
/// The 03:05 default is deliberately five minutes AFTER the 03:00 daily backup
/// fires: the backup already holds /run/das-maintenance.lock, so the scrub
/// engine's blocking acquire starts the scrub the moment the backup finishes
/// — "scrub follows the backup on the 1st" (user decision 2026-08-02,
/// superseding the fixed-05:30 gap design of 2026-07-27). In the other
/// direction a backup no longer waits behind a running pass (operator
/// 2026-11-01, bd DAS-Backup-Manager-su1b.2): the backup claims the lock,
/// the pass stops its scrub, unmounts and gives the lock up, and resumes the
/// same filesystem once the backup is done — the 03:00 backup and, on a
/// Sunday the 1st, the 04:00 full backup each run within about a minute. NO
/// `RandomizedDelaySec`: the maintenance lock — not timing — is the
/// serialization mechanism.
pub fn render_systemd_scrub_timer(config: &Config) -> String {
    let on_calendar = &config.scrub.on_calendar;

    format!(
        "{GENERATED_HEADER}\n\
         [Unit]\n\
         Description=DAS Backup - Monthly BTRFS scrub timer\n\
         \n\
         [Timer]\n\
         OnCalendar={on_calendar}\n\
         Persistent=true\n\
         WakeSystem=false\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

/// Generate the das-backup-doctor.service unit file. `Type=oneshot`, no
/// `Restart=` (Sentinel — `cachyos-sentinel` — handles retries for a unit
/// that lands in `failed` state; the unit itself stays as clean as
/// `das-backup.service`/`das-scrub.service`, per the project's established
/// convention of leaving retry policy entirely to Sentinel rather than
/// duplicating it in the unit).
///
/// `ExecStart` is preceded by a comment documenting the exit-code split (bd
/// DAS-Backup-Manager-01u, mirroring das-scrub.service's precedent for bd
/// DAS-Backup-Manager-18p): unlike scrub, a drift check genuinely wants exit
/// 1 to mean "drift found" — it is a fast, read-mostly scan with no
/// multi-hour-retry hazard, so there is no reason to hide findings from the
/// exit code the way scrub does.
///
/// `TimeoutStartSec=600` (not `infinity` like das-backup/das-scrub): those
/// units use a *blocking* maintenance-lock acquisition that can legitimately
/// wait hours behind a real backup or scrub pass, so a finite timeout would
/// `SIGKILL` them mid-wait. `btrdasd doctor` deliberately acquires both its
/// locks non-blocking (`doctor::try_acquire_locks`) and defers immediately
/// (exit 0) rather than queuing — so it never has a legitimate reason to run
/// long, and a bound here is a real safety net against a hung `btrfs`
/// subprocess rather than fighting the unit's own design.
///
/// The 600 s grows by `[das].member_wait_secs` for every RAID-1 target (bd
/// DAS-Backup-Manager-su1b.11): the doctor mounts each through the guard,
/// which waits that long for a late member before deciding, and a bound that
/// fired inside the wait would kill a check doing what it should.
pub fn render_systemd_doctor_service(config: &Config) -> String {
    let prefix = &config.general.install_prefix;
    let raid_targets = config
        .targets
        .iter()
        .filter(|t| buttered_dasd::raid_guard::is_guarded(t))
        .count() as u64;
    let timeout = 600 + raid_targets * u64::from(config.das.member_wait_secs);

    format!(
        "{GENERATED_HEADER}\n\
         [Unit]\n\
         Description=DAS Backup - Weekly subvolume drift check\n\
         After=local-fs.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         # Exit semantics (bd DAS-Backup-Manager-01u, refined by -f6p):\n\
         #   0 = ran clean, or deferred because a backup/scrub/another doctor run\n\
         #       holds a lock (never a failure).\n\
         #   1 = DRIFT FOUND — missing or stale subvolumes, or a declared pair a\n\
         #       target holds no recent snapshot of (bd -sp78.7). A successful check\n\
         #       WITH A RESULT, not a malfunction, which is why SuccessExitStatus\n\
         #       below tells systemd not to fail the unit for it. The finding\n\
         #       travels by email (--email) and by the journal report.\n\
         #   3 = at least one configured volume failed to mount/list while others\n\
         #       were checked — those subvolumes went unexamined — or a volume\n\
         #       the check mounted could not be unmounted again, or a target the\n\
         #       delivery check could not read. An operational\n\
         #       fault, so the unit DOES fail. Outranks 1 when both occur: an\n\
         #       incomplete check cannot claim its drift list is complete.\n\
         #   2 = could not run at all — every configured source volume failed to\n\
         #       mount or list, so nothing was ever examined (config load failure\n\
         #       and lock I/O errors also land here, via the CLI's own error path\n\
         #       rather than this exit-code mapping).\n\
         #\n\
         # Without SuccessExitStatus=1 systemd cannot tell a finding from a\n\
         # malfunction: it marked this unit `failed` on drift, cachyos-sentinel\n\
         # then restarted it and notified 'backup service das-backup-doctor\n\
         # .service last run failed', so the operator was told the checker broke\n\
         # at the moment it worked — an alert that contradicted the drift email\n\
         # carrying the real signal (observed 2026-08-31 13:47).\n\
         SuccessExitStatus=1\n\
         ExecStart={prefix}/bin/btrdasd doctor --check-drift --email\n\
         StandardOutput=journal\n\
         StandardError=journal\n\
         Nice=19\n\
         IOSchedulingClass=idle\n\
         # 600 s, plus [das].member_wait_secs for each RAID-1 target.\n\
         TimeoutStartSec={timeout}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Generate the das-backup-doctor.timer unit file. Fixed `Sun 02:00` — before
/// the Sunday full backup (04:00) and monthly scrub (03:05 on the 1st), so a
/// drift finding is visible ahead of that week's backup window rather than
/// discovered after the fact. No `RandomizedDelaySec` — like das-scrub.timer,
/// the fixed time is deliberate and the maintenance lock (which this check
/// acquires non-blocking and defers on, never queuing) is the safety net
/// against overlap, not timing.
pub fn render_systemd_doctor_timer(_config: &Config) -> String {
    format!(
        "{GENERATED_HEADER}\n\
         [Unit]\n\
         Description=DAS Backup - Weekly subvolume drift check timer\n\
         \n\
         [Timer]\n\
         OnCalendar=Sun *-*-* 02:00:00\n\
         Persistent=true\n\
         WakeSystem=false\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

/// Generate cron entries for incremental (daily) and full (weekly) backups.
pub fn render_cron_entry(config: &Config) -> Result<String, String> {
    let script_dir = format!("{}/lib/das-backup", config.general.install_prefix);

    // Parse "03:00" -> hour=3, minute=0
    let (inc_hour, inc_min) = parse_time(&config.schedule.incremental)?;

    // Parse "Sun 04:00" -> dow=0, hour=4, minute=0
    let (full_dow, full_hour, full_min) = parse_schedule_with_day(&config.schedule.full)?;

    Ok(format!(
        "{GENERATED_HEADER}\
         # Incremental backup (daily)\n\
         {inc_min} {inc_hour} * * * root {script_dir}/backup-run.sh \
         >> /var/log/das-backup.log 2>&1\n\
         \n\
         # Full backup (weekly)\n\
         {full_min} {full_hour} * * {full_dow} root {script_dir}/backup-run.sh --full \
         >> /var/log/das-backup.log 2>&1\n"
    ))
}

// Email config file generation removed 2026-05-16, and there is nothing to
// bring back: since the 2026-08-06 relay migration this project holds no mail
// credential at all. Reports are submitted unauthenticated to the local relay
// at `[email].smtp_host:smtp_port`, which authenticates upstream by envelope
// sender using a key readable only by root. Both senders
// (`scripts/backup-run.sh::send_report` and
// `indexer/src/report.rs::send_email_report`) take their coordinates from
// `[email]` in config.toml — no project-local email config file exists.

// ESP sync hook generation removed 2026-04-10 — root cause of the 2026-03-05
// incident that wiped the DAS 2TB emergency recovery drives' independent OS
// ESPs. NVMe-pair ESP mirroring is handled by a separate, unrelated mechanism
// (/usr/local/bin/esp-sync.sh + /etc/pacman.d/hooks/esp-mirror.hook) outside
// this project. See .claude/rules/esp-safety.md for the full postmortem.

/// Where the udisks-ignore rule is installed.
pub const UDEV_UDISKS_IGNORE_RULES: &str = "/etc/udev/rules.d/99-das-backup-udisks-ignore.rules";

/// True when `value` can be interpolated into a udev `=="..."` match without
/// changing what the rule means: no quote to end the string early, and none
/// of udev's glob characters (`*?[]|`) to widen the match.
pub fn is_udev_safe_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Generate the udev rule that hides every backup target from udisks2, and so
/// from desktop automounters (bd `DAS-Backup-Manager-a10`, `-xqo`).
///
/// A target left visible is mounted read-write under `/run/media/<user>` at
/// login — a second door onto a filesystem only `backup-run.sh` and `btrdasd`
/// are meant to open. A multi-device target is worse: btrfs reports one member
/// as the mount source, udisks believes the others are still unmounted, and
/// each further login mounts them again.
///
/// Keyed on each configured drive serial, scoped to the btrfs partition so a
/// recovery drive's own ESP is not matched, and additionally on `mount_uuid`
/// where one is configured, which covers a replacement leg whose serial has
/// not reached the config yet. A hand-written predecessor was keyed on the
/// UUID alone and matched nothing for months after the filesystem was
/// re-created; rendering from `config.toml` ties this rule to the identifiers
/// the backup itself mounts by, so it cannot drift from them unnoticed.
pub fn render_udev_udisks_ignore(config: &Config) -> String {
    let mut out = format!(
        "{GENERATED_HEADER}#\n\
         # Hides the backup targets from udisks2 so no desktop session automounts them.\n\
         # Verify with: sudo btrdasd setup --check\n"
    );
    for target in &config.targets {
        out.push_str(&format!("\n# {}\n", target.label.replace('\n', " ")));
        for serial in target.effective_serials() {
            if is_udev_safe_value(&serial) {
                out.push_str(&format!(
                    "SUBSYSTEM==\"block\", ENV{{ID_SERIAL_SHORT}}==\"{serial}\", \
                     ENV{{ID_FS_TYPE}}==\"btrfs\", ENV{{UDISKS_IGNORE}}=\"1\"\n"
                ));
            } else {
                out.push_str("# SKIPPED a serial that is not safe to place in a udev match\n");
            }
        }
        if let Some(uuid) = &target.mount_uuid {
            if is_udev_safe_value(uuid) {
                out.push_str(&format!(
                    "SUBSYSTEM==\"block\", ENV{{ID_FS_UUID}}==\"{uuid}\", \
                     ENV{{UDISKS_IGNORE}}=\"1\"\n"
                ));
            } else {
                out.push_str("# SKIPPED a mount_uuid that is not safe to place in a udev match\n");
            }
        }
    }
    out.push_str(&render_recovery_esp_names(config));
    out
}

/// The PARTUUIDs of the configured recovery ESPs as udev reports them (lower
/// case), each `Ok` when it is a PARTUUID and `Err(raw)` when it is not. A
/// value is checked with `config::is_partuuid` before it can reach a rule.
pub fn recovery_esp_partuuids(config: &Config) -> Vec<Result<String, String>> {
    config
        .targets
        .iter()
        .filter_map(|t| t.esp_partuuid.as_deref())
        .map(|pu| {
            if buttered_dasd::config::is_partuuid(pu) {
                Ok(pu.to_ascii_lowercase())
            } else {
                Err(pu.to_string())
            }
        })
        .collect()
}

/// The rule lines that name each configured recovery ESP `RECOVERY` in udev
/// (bd `DAS-Backup-Manager-eg4j.18`). fwupd enumerates every ESP by partition
/// type and mounts each read-write as root; it skips one whose partition name
/// is on udisks' recovery list (`fu_volume_check_is_recovery`). Only a udev
/// property changes: the on-disk GPT is never written. Keyed on the
/// configured PARTUUID alone, never on type or discovery (`esp-safety.md`).
fn render_recovery_esp_names(config: &Config) -> String {
    let uuids = recovery_esp_partuuids(config);
    if uuids.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n# Recovery ESPs: name the partition RECOVERY in udev so fwupd (which skips an ESP\n\
         # whose name is on udisks' recovery list, fu_volume_check_is_recovery) never mounts\n\
         # one. A udev property only; the on-disk partition table is untouched.\n\
         # bd DAS-Backup-Manager-eg4j.18\n",
    );
    for uuid in uuids {
        match uuid {
            Ok(uuid) => out.push_str(&format!(
                "ACTION!=\"remove\", SUBSYSTEM==\"block\", ENV{{DEVTYPE}}==\"partition\", \
                 ENV{{ID_PART_ENTRY_UUID}}==\"{uuid}\", ENV{{ID_PART_ENTRY_NAME}}=\"RECOVERY\"\n"
            )),
            Err(_) => out.push_str("# SKIPPED an esp_partuuid that is not a PARTUUID\n"),
        }
    }
    out
}

/// Where the I/O-scheduler rule is installed. Sorts after the host's generic
/// `61-*` rotational-disk rules, so its match wins on the DAS drives only.
pub const UDEV_IO_SCHEDULER_RULES: &str = "/etc/udev/rules.d/99-das-backup-io-scheduler.rules";

/// Generate the udev rule that pins `[das].io_scheduler` on every backup
/// target drive at every add/change event (bd `DAS-Backup-Manager-su1b.4`).
///
/// `backup-run.sh` sets the scheduler only while a run is going, so after a
/// USB re-plug, an enclosure power cycle or a boot the drives sit on whatever
/// the host's generic rotational-disk rule chose (often bfq) until the next
/// backup. This file sorts after such rules (`99-`), so its match wins, on the
/// configured serials only. Keyed on `ID_SERIAL_SHORT` like the udisks rule,
/// scoped to the whole disk (`DEVTYPE=disk`: `queue/scheduler` exists there,
/// not on a partition).
///
/// `None` when `[das].io_scheduler` is empty or not one of
/// [`buttered_dasd::config::IO_SCHEDULERS`]: no rule, so setup writes none and
/// removes a stale one ([`GeneratedFiles::generate`] refuses the latter).
pub fn render_udev_io_scheduler(config: &Config) -> Option<String> {
    let scheduler = config.das.io_scheduler.as_str();
    if !buttered_dasd::config::IO_SCHEDULERS.contains(&scheduler) {
        return None;
    }
    // A scheduler switch resets queue/nr_requests, so it is written after
    // the scheduler on the same line; udev applies a line's assignments in
    // order (bd DAS-Backup-Manager-su1b.8). 0 leaves it alone; an out-of-range
    // value never reaches the rule (Config::validate and generate refuse it).
    let nr = config.das.nr_requests;
    let nr_attr = if (buttered_dasd::config::NR_REQUESTS_MIN
        ..=buttered_dasd::config::NR_REQUESTS_MAX)
        .contains(&nr)
    {
        format!(", ATTR{{queue/nr_requests}}=\"{nr}\"")
    } else {
        String::new()
    };
    let mut out = format!(
        "{GENERATED_HEADER}#\n\
         # Pins the I/O scheduler of the backup target drives ([das].io_scheduler in\n\
         # config.toml), then their request queue size ([das].nr_requests; a\n\
         # scheduler switch resets it), at every add/change event; do not edit,\n\
         # change the keys and re-run: sudo btrdasd setup --upgrade.\n\
         # Verify with: sudo btrdasd setup --check\n"
    );
    let mut seen: Vec<String> = Vec::new();
    for target in &config.targets {
        out.push_str(&format!("\n# {}\n", target.label.replace('\n', " ")));
        for serial in target.effective_serials() {
            if !is_udev_safe_value(&serial) {
                out.push_str("# SKIPPED a serial that is not safe to place in a udev match\n");
            } else if !seen.contains(&serial) {
                out.push_str(&format!(
                    "ACTION==\"add|change\", SUBSYSTEM==\"block\", ENV{{DEVTYPE}}==\"disk\", \
                     ENV{{ID_SERIAL_SHORT}}==\"{serial}\", ATTR{{queue/scheduler}}=\"{scheduler}\"{nr_attr}\n"
                ));
                seen.push(serial);
            }
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// GeneratedFiles — aggregates all rendered files
// ---------------------------------------------------------------------------

/// Collection of all generated files as (path, content) pairs.
pub struct GeneratedFiles {
    pub files: Vec<(String, String)>,
}

impl GeneratedFiles {
    /// Generate all operational files from a Config. Includes systemd units OR cron
    /// entries based on init system, plus btrbk.conf, the backup scripts and the udisks-ignore
    /// udev rule. No email config and no ESP hook is generated.
    ///
    /// `Err` when the renderer refuses the config (a volume's top level in a
    /// source that reaches a mirror target, bd xtnq): nothing is generated,
    /// so `setup` writes nothing, although it does not run `validate`.
    pub fn generate(config: &Config) -> Result<Self, String> {
        let script_dir = format!("{}/lib/das-backup", config.general.install_prefix);
        let mut files = Vec::new();

        // btrbk.conf — at `general.btrbk_conf` (canonical /etc/btrbk/btrbk.conf),
        // the path every other reader and writer of it uses.
        files.push((
            config.general.btrbk_conf.clone(),
            render_btrbk_conf(config)?,
        ));
        // Its companion for the entries sent after the indexer ran (bd 9eq9),
        // beside it; present only when some entry is flagged `after_index`.
        if let Some(state) = render_state_conf(config)? {
            files.push((
                buttered_dasd::btrbk_conf::state_conf_path(config)
                    .to_string_lossy()
                    .into_owned(),
                state,
            ));
        }

        // Real production scripts (embedded in binary, config-driven via eval),
        // installed flat — matches cmake's install layout (single canonical
        // location per Single Canonical Source, ~/.claude/rules/development-tools.md)
        files.push((
            format!("{script_dir}/backup-run.sh"),
            SCRIPT_BACKUP_RUN.to_string(),
        ));
        files.push((
            format!("{script_dir}/backup-verify.sh"),
            SCRIPT_BACKUP_VERIFY.to_string(),
        ));
        files.push((
            format!("{script_dir}/boot-archive-cleanup.sh"),
            SCRIPT_BOOT_ARCHIVE_CLEANUP.to_string(),
        ));

        // Scheduling: systemd OR cron
        match config.init.system {
            InitSystem::Systemd => {
                files.push((
                    "/etc/systemd/system/das-backup.service".to_string(),
                    render_systemd_service(config, false),
                ));
                files.push((
                    "/etc/systemd/system/das-backup.timer".to_string(),
                    render_systemd_timer(config, false),
                ));
                files.push((
                    "/etc/systemd/system/das-backup-full.service".to_string(),
                    render_systemd_service(config, true),
                ));
                files.push((
                    "/etc/systemd/system/das-backup-full.timer".to_string(),
                    render_systemd_timer(config, true),
                ));

                // Scrub unit + timer always installed; whether the timer is
                // *enabled* is gated on `config.scrub.enabled` in
                // `installer::install()` (the engine ignores `enabled` for
                // manual `btrdasd scrub run` and only warns — the timer is
                // the sole enforcement point, bd DAS-Backup-Manager-atq).
                files.push((
                    "/etc/systemd/system/das-scrub.service".to_string(),
                    render_systemd_scrub_service(config),
                ));
                files.push((
                    "/etc/systemd/system/das-scrub.timer".to_string(),
                    render_systemd_scrub_timer(config),
                ));

                // Doctor unit + timer always installed and always enabled —
                // unlike scrub, there is no config toggle for the drift check
                // (it has no meaningful "disabled" state to gate on: it is a
                // read-mostly scan, not a write operation with resource cost
                // worth opting out of). See `installer::install()`.
                files.push((
                    "/etc/systemd/system/das-backup-doctor.service".to_string(),
                    render_systemd_doctor_service(config),
                ));
                files.push((
                    "/etc/systemd/system/das-backup-doctor.timer".to_string(),
                    render_systemd_doctor_timer(config),
                ));
            }
            InitSystem::Sysvinit | InitSystem::Openrc => {
                // Fail closed: an unparsable schedule yields NO cron file
                // rather than one silently pinned to 03:00 Sunday. Unreachable
                // via the normal flow — `Config::validate()` rejects a bad
                // schedule before setup ever generates anything
                // (bd DAS-Backup-Manager-06p).
                match render_cron_entry(config) {
                    Ok(entry) => files.push(("/etc/cron.d/das-backup".to_string(), entry)),
                    Err(e) => eprintln!(
                        "Refusing to generate /etc/cron.d/das-backup — {e}. \
                         Fix [schedule] in config.toml and re-run setup."
                    ),
                }
            }
        }

        // Hide the targets from udisks2. Independent of the init system.
        files.push((
            UDEV_UDISKS_IGNORE_RULES.to_string(),
            render_udev_udisks_ignore(config),
        ));

        // Pin the I/O scheduler on the targets' drives, between runs too
        // (bd su1b.4). Unset means no rule; a stale one then drops out of the
        // manifest and the upgrade removes it. A value outside the safe set
        // is refused, as `Config::validate` does.
        let nr = config.das.nr_requests;
        if nr != 0
            && !(buttered_dasd::config::NR_REQUESTS_MIN..=buttered_dasd::config::NR_REQUESTS_MAX)
                .contains(&nr)
        {
            return Err(format!(
                "das.nr_requests {nr} must be 0 or {}..={}",
                buttered_dasd::config::NR_REQUESTS_MIN,
                buttered_dasd::config::NR_REQUESTS_MAX
            ));
        }
        if !config.das.io_scheduler.is_empty() {
            let rule = render_udev_io_scheduler(config).ok_or_else(|| {
                format!(
                    "das.io_scheduler {:?} must be one of {}, or empty",
                    config.das.io_scheduler,
                    buttered_dasd::config::IO_SCHEDULERS.join(", ")
                )
            })?;
            files.push((UDEV_IO_SCHEDULER_RULES.to_string(), rule));
        }

        // No email config file is generated: this project stores no mail
        // credential. See the note above `render_cron_entry`'s neighbours and
        // `.claude/rules/backup.md` §Email Reports.

        Ok(Self { files })
    }

    /// Produce a newline-joined manifest of all generated file paths.
    pub fn manifest_content(&self) -> String {
        self.files
            .iter()
            .map(|(p, _)| p.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests (TDD — written first, implementation follows)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn test_config() -> Config {
        let mut config = Config::default();
        config.general.install_prefix = "/usr/local".to_string();
        config.sources.push(Source {
            label: "nvme-root".to_string(),
            volume: "/.btrfs-nvme".to_string(),
            subvolumes: vec![
                SubvolConfig {
                    name: "@".to_string(),
                    snapshot_name: None,
                    ..Default::default()
                },
                SubvolConfig {
                    name: "@home".to_string(),
                    snapshot_name: None,
                    ..Default::default()
                },
            ],
            device: "/dev/nvme0n1p2".to_string(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec!["nvme".into()],
            target_labels: vec![],
            static_subvolumes: Vec::new(),
        });
        config.targets.push(Target {
            label: "primary-22tb".to_string(),
            serial: "SERIAL08".to_string(),
            serials: vec!["SERIAL08".to_string()],
            mount_uuid: None,
            mount: "/mnt/backup-22tb".to_string(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 0,
                yearly: 0,
            },
            display_name: String::new(),
            esp_partuuid: None,
        });
        config
    }

    #[test]
    fn render_systemd_service_full_unit_test() {
        let config = test_config();
        assert_eq!(
            render_systemd_service(&config, true),
            concat!(
                "# Generated by btrdasd setup — do not edit.\n",
                "# Modify /etc/das-backup/config.toml and run: sudo btrdasd setup --upgrade\n",
                "\n",
                "[Unit]\n",
                "Description=DAS Backup - Full BTRFS backup\n",
                "After=local-fs.target\n",
                "Wants=time-sync.target\n",
                "After=time-sync.target\n",
                "\n",
                "[Service]\n",
                "Type=oneshot\n",
                "# Exit semantics (bd DAS-Backup-Manager-d1r):\n",
                "#   0 = the run executed and nothing failed (a warning still exits 0), or it\n",
                "#       skipped because another backup was running.\n",
                "#   3 = the run began its work and something failed: btrbk failed for some or\n",
                "#       all targets, any operation failed, or it aborted on a target's or a\n",
                "#       source's state. Its report says what (an abort sends a short ABORTED\n",
                "#       report), the history has it as failed, and the journal has status=3.\n",
                "#   1 = could not start: nothing was mounted or sent (config, btrdasd missing,\n",
                "#       arguments, not root, a lock it could not open or take).\n",
                "#   129 130 138 141 142 143 = stopped by a signal (HUP INT USR1 PIPE ALRM TERM).\n",
                "# SuccessExitStatus=3: what made a run fail is usually still there ten minutes\n",
                "# later (an absent drive). With the unit failed, cachyos-sentinel would start\n",
                "# a whole new backup about every ten minutes; its 3-per-600 s limiter cannot\n",
                "# brake a loop that slow. Exit 1 still fails the unit, in seconds.\n",
                "SuccessExitStatus=3\n",
                "ExecStart=/usr/local/lib/das-backup/backup-run.sh --full\n",
                "StandardOutput=journal\n",
                "StandardError=journal\n",
                "Nice=19\n",
                "IOSchedulingClass=idle\n",
                "TimeoutStartSec=infinity\n",
                "\n",
                "[Install]\n",
                "WantedBy=multi-user.target\n",
            )
        );
    }

    #[test]
    fn render_systemd_timer_full_puts_the_weekday_ahead_of_the_date() {
        // systemd's OnCalendar grammar is `DayOfWeek Date Time`. "Sun 04:00"
        // from config.toml has to become "Sun *-*-* 04:00:00"; the weekday
        // anywhere else is rejected by systemd and the weekly full backup
        // never fires.
        let config = test_config();
        assert_eq!(config.schedule.full, "Sun 04:00");
        assert_eq!(
            render_systemd_timer(&config, true),
            concat!(
                "# Generated by btrdasd setup — do not edit.\n",
                "# Modify /etc/das-backup/config.toml and run: sudo btrdasd setup --upgrade\n",
                "\n",
                "[Unit]\n",
                "Description=DAS Backup Timer - Weekly full backup\n",
                "\n",
                "[Timer]\n",
                "OnCalendar=Sun *-*-* 04:00:00\n",
                "RandomizedDelaySec=1800\n",
                "Persistent=true\n",
                "WakeSystem=false\n",
                "\n",
                "[Install]\n",
                "WantedBy=timers.target\n",
            )
        );
    }

    #[test]
    fn render_systemd_timer_full_without_a_weekday_fires_daily_at_that_time() {
        let mut config = test_config();
        config.schedule.full = "04:30".to_string();
        let result = render_systemd_timer(&config, true);
        assert!(result.contains("\nOnCalendar=*-*-* 04:30:00\n"), "{result}");
    }

    #[test]
    fn render_systemd_service_test() {
        let config = test_config();
        let result = render_systemd_service(&config, false);
        assert!(result.contains("ExecStart=/usr/local/lib/das-backup/backup-run.sh"));
        // Every command runs from the install prefix, never from a checkout.
        for line in result.lines().filter(|l| l.starts_with("ExecStart")) {
            let cmd = line.split_once('=').map_or("", |(_, c)| c);
            assert!(
                cmd.trim_start_matches(['-', '+', '@', '!', ':'])
                    .starts_with("/usr/local/"),
                "{line}"
            );
        }
        assert!(result.contains("# Generated by btrdasd setup"));
    }

    /// The `SuccessExitStatus=` lines of a unit, each with the section it sits in.
    fn success_exit_status_lines(unit: &str) -> Vec<String> {
        let mut section = "";
        let mut found = Vec::new();
        for line in unit.lines() {
            if line.starts_with('[') {
                section = line;
            } else if line.starts_with("SuccessExitStatus=") {
                found.push(format!("{section} {line}"));
            }
        }
        found
    }

    #[test]
    fn render_systemd_service_counts_exit_3_as_success_test() {
        // bd DAS-Backup-Manager-d1r: backup-run.sh exits 3 when the run began
        // its work and something failed. Unless the unit counts 3 as success
        // it ends `failed`, and cachyos-sentinel starts a whole new backup
        // about every ten minutes for as long as the cause lasts — one absent
        // drive is enough. Exactly 3: exit 1 (could not start) must still
        // fail the unit.
        let config = test_config();
        for full in [false, true] {
            let unit = render_systemd_service(&config, full);
            assert_eq!(
                success_exit_status_lines(&unit),
                ["[Service] SuccessExitStatus=3"],
                "full={full}:\n{unit}"
            );
            // The unit says why, so `systemctl cat` explains a status=3.
            assert!(unit.contains("# Exit semantics"), "full={full}");
            assert!(unit.contains("bd DAS-Backup-Manager-d1r"), "full={full}");
        }
    }

    #[test]
    fn render_systemd_service_timeout_is_unbounded_test() {
        // bd DAS-Backup-Manager-b6f: a backup can defer for hours behind a
        // running scrub pass via the shared maintenance lock, so the unit
        // must never be killed by a finite TimeoutStartSec.
        let config = test_config();
        let incremental = render_systemd_service(&config, false);
        let full = render_systemd_service(&config, true);
        assert!(incremental.contains("TimeoutStartSec=infinity"));
        assert!(full.contains("TimeoutStartSec=infinity"));
        assert!(!incremental.contains("TimeoutStartSec=21600"));
        assert!(!full.contains("TimeoutStartSec=21600"));
    }

    #[test]
    fn render_systemd_timer_test() {
        let config = test_config();
        let result = render_systemd_timer(&config, false);
        assert!(result.contains("OnCalendar=*-*-* 03:00:00"));
        assert!(result.contains("RandomizedDelaySec=1800"));
    }

    #[test]
    fn render_systemd_scrub_service_test() {
        let config = test_config();
        let result = render_systemd_scrub_service(&config);
        assert!(result.contains("\nWants=time-sync.target\nAfter=time-sync.target\n"));
        assert!(result.contains("Type=oneshot"));
        assert!(result.contains("ExecStart=/usr/local/bin/btrdasd scrub run"));
        assert!(result.contains("TimeoutStartSec=infinity"));
        assert!(result.contains("# Generated by btrdasd setup"));
        // bd DAS-Backup-Manager-su1b.1: the unit itself stays best-effort level
        // 0, nice -20; never idle, never real-time (the workers' real-time
        // class, su1b.5, is chosen on the btrfs argv, not here).
        assert!(
            result.contains("\nNice=-20\nIOSchedulingClass=best-effort\nIOSchedulingPriority=0\n")
        );
        assert!(!result.contains("Nice=19"));
        assert!(!result.contains("IOSchedulingClass=idle"));
        assert!(!result.contains("IOSchedulingClass=realtime"));
        // bd DAS-Backup-Manager-18p: the unit documents its own exit-code
        // split so a reader of `systemctl cat das-scrub.service` sees why
        // findings don't fail the unit, without needing to go read the
        // engine source.
        assert!(result.contains("# Exit semantics"));
        assert!(result.contains("bd DAS-Backup-Manager-18p"));
        // Explicitly scrapped by user decision 2026-07-27 — deferral via the
        // maintenance lock replaces unit-layer cancellation.
        assert!(!result.contains("Conflicts="));
        assert!(!result.contains("RuntimeMaxSec="));
        assert!(!result.contains("ExecStopPost"));
    }

    #[test]
    fn render_systemd_scrub_timer_test() {
        let config = test_config();
        let result = render_systemd_scrub_timer(&config);
        assert!(result.contains("OnCalendar=*-*-01 03:05:00"));
        assert!(result.contains("Persistent=true"));
        // 03:05 deliberately trails the 03:00 backup so the maintenance lock
        // serializes scrub-after-backup; the lock, not randomized timing, is
        // the mechanism.
        assert!(!result.contains("RandomizedDelaySec"));
    }

    #[test]
    fn render_systemd_scrub_timer_uses_configured_calendar_test() {
        let mut config = test_config();
        config.scrub.on_calendar = "*-*-15 02:00:00".to_string();
        let result = render_systemd_scrub_timer(&config);
        assert!(result.contains("OnCalendar=*-*-15 02:00:00"));
    }

    #[test]
    fn render_systemd_doctor_service_test() {
        let config = test_config();
        let result = render_systemd_doctor_service(&config);
        assert!(result.contains("Type=oneshot"));
        assert!(result.contains("ExecStart=/usr/local/bin/btrdasd doctor --check-drift --email"));
        assert!(result.contains("# Generated by btrdasd setup"));
        // bd DAS-Backup-Manager-01u: the unit documents its own exit-code
        // split, mirroring das-scrub.service's precedent.
        assert!(result.contains("# Exit semantics"));
        assert!(result.contains("bd DAS-Backup-Manager-01u"));
        // Exit 3 is also a volume the check could not unmount again
        // (doctor::report.left_mounted); the unit's comment must say so.
        let exit3 = result.split("#   3 = ").nth(1).expect("exit 3 row");
        let exit3 = exit3.split("#   2 = ").next().unwrap();
        assert!(exit3.contains("unmounted again"), "{exit3}");
        // Deliberately finite (unlike das-backup/das-scrub's infinity) — the
        // doctor's own lock acquisition is non-blocking, so it never has a
        // legitimate reason to run long.
        assert!(result.contains("TimeoutStartSec=600\n"));
        assert!(!result.contains("TimeoutStartSec=infinity"));
    }

    /// bd DAS-Backup-Manager-su1b.11: the doctor mounts each RAID-1 target
    /// through the guard, which may wait `[das].member_wait_secs` for a late
    /// member; a fixed 600 s bound would kill it mid-wait (the unit failed,
    /// a supervisor restarting it). The bound is 600 s plus that wait for
    /// every RAID-1 target.
    #[test]
    fn the_doctor_unit_bound_covers_the_raid_member_wait() {
        let mut config = test_config();
        let mut raid = config.targets[0].clone();
        raid.label = "raid".into();
        raid.mount_uuid = Some("5eed0016-0000-4000-8000-000000000016".into());
        raid.serials = vec!["SERIAL11".into(), "SERIAL12".into()];
        config.targets.push(raid);
        config.das.member_wait_secs = 900;
        let result = render_systemd_doctor_service(&config);
        assert!(result.contains("TimeoutStartSec=1500\n"), "{result}");
        config.das.member_wait_secs = 3600;
        let two = {
            let mut c = config.clone();
            let mut second = c.targets.last().unwrap().clone();
            second.label = "raid2".into();
            c.targets.push(second);
            c
        };
        assert!(render_systemd_doctor_service(&two).contains("TimeoutStartSec=7800\n"));
    }

    #[test]
    fn render_systemd_doctor_timer_test() {
        let config = test_config();
        let result = render_systemd_doctor_timer(&config);
        assert!(result.contains("OnCalendar=Sun *-*-* 02:00:00"));
        assert!(result.contains("Persistent=true"));
        // Fixed Sunday 02:00, ahead of the Sunday full backup (04:00) and
        // monthly scrub (05:30 on the 1st) — no RandomizedDelaySec.
        assert!(!result.contains("RandomizedDelaySec"));
    }

    #[test]
    fn render_cron_entry_test() {
        let config = test_config();
        let result = render_cron_entry(&config).expect("valid schedule renders");
        assert!(result.contains("0 3 * * *"));
        assert!(result.contains("/usr/local/lib/das-backup/backup-run.sh"));
    }

    // Counter-tests for bd DAS-Backup-Manager-06p. Each of these produced a
    // silently-defaulted cron entry before the parsers returned Result:
    // "3 AM" became 03:00, and "Friday 04:00" became SUNDAY 04:00.
    #[test]
    fn render_cron_entry_refuses_an_unparsable_time() {
        let mut config = test_config();
        config.schedule.incremental = "3 AM".into();
        let err = render_cron_entry(&config).expect_err("must not silently default to 03:00");
        assert!(err.contains("3 AM"), "{err}");
    }

    #[test]
    fn render_cron_entry_refuses_a_full_weekday_name() {
        let mut config = test_config();
        config.schedule.full = "Friday 04:00".into();
        let err = render_cron_entry(&config).expect_err("must not silently default to Sunday");
        assert!(err.contains("friday"), "{err}");
    }

    #[test]
    fn schedule_errors_are_surfaced_by_config_validate() {
        let mut config = test_config();
        config.schedule.incremental = "0300".into();
        config.schedule.full = "Funday 04:00".into();
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("schedule.incremental")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("schedule.full")),
            "{errors:?}"
        );
    }

    #[test]
    fn generated_files_include_embedded_scripts() {
        let config = test_config();
        let generated = GeneratedFiles::generate(&config).unwrap();
        let manifest = generated.manifest_content();
        assert!(
            manifest.contains("/usr/local/lib/das-backup/backup-run.sh"),
            "missing embedded backup-run.sh at flat path"
        );
        assert!(
            manifest.contains("/usr/local/lib/das-backup/backup-verify.sh"),
            "missing embedded backup-verify.sh at flat path"
        );
        assert!(
            manifest.contains("/usr/local/lib/das-backup/boot-archive-cleanup.sh"),
            "missing embedded boot-archive-cleanup.sh at flat path"
        );
        assert!(
            !manifest.contains("das-backup/scripts/"),
            "scripts/ subdir layout must not reappear — flat layout is canonical"
        );
    }

    fn with_state_entry(mut config: Config) -> Config {
        config.sources[0].subvolumes.push(SubvolConfig {
            name: "@/var/lib/das-backup".to_string(),
            snapshot_name: Some("das-state".to_string()),
            after_index: true,
            ..Default::default()
        });
        config
    }

    fn generated_text<'a>(g: &'a GeneratedFiles, path: &str) -> Option<&'a str> {
        g.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, t)| t.as_str())
    }

    #[test]
    fn an_after_index_entry_generates_the_state_conf_beside_the_main_one() {
        let config = with_state_entry(test_config());
        let generated = GeneratedFiles::generate(&config).unwrap();
        let main = generated_text(&generated, "/etc/btrbk/btrbk.conf").unwrap();
        let state = generated_text(&generated, "/etc/btrbk/btrbk-state.conf")
            .expect("the state conf is generated");
        assert!(!main.contains("das-state"), "{main}");
        assert!(state.contains("snapshot_name       das-state"), "{state}");
    }

    #[test]
    fn both_conf_paths_follow_general_btrbk_conf() {
        let mut config = with_state_entry(test_config());
        config.general.btrbk_conf = "/srv/btrbk/main.conf".to_string();
        let generated = GeneratedFiles::generate(&config).unwrap();
        assert!(generated_text(&generated, "/srv/btrbk/main.conf").is_some());
        assert!(generated_text(&generated, "/srv/btrbk/btrbk-state.conf").is_some());
        assert!(generated_text(&generated, "/etc/btrbk/btrbk.conf").is_none());
    }

    /// `test_config()` plus a mirror target and the volume's top level in
    /// `nvme-root`, which sends to every target — a widened adoption source.
    pub(crate) fn top_level_to_a_mirror() -> Config {
        let mut config = test_config();
        let mut mirror = config.targets[0].clone();
        mirror.label = "recovery-a".into();
        mirror.mount = "/mnt/recovery-a".into();
        mirror.role = TargetRole::Mirror;
        config.targets.push(mirror);
        config.sources[0].subvolumes.push(SubvolConfig {
            name: buttered_dasd::adopt::TOP_LEVEL.into(),
            snapshot_name: Some("btrfs-nvme-toplevel".into()),
            ..Default::default()
        });
        config
    }

    /// bd xtnq D1: `setup --upgrade` renders through here without `validate`.
    #[test]
    fn generate_refuses_a_top_level_that_reaches_a_mirror() {
        let why = GeneratedFiles::generate(&top_level_to_a_mirror())
            .err()
            .expect("refused");
        assert!(
            why.contains("'nvme-root'") && why.contains("recovery-a"),
            "{why}"
        );
    }

    #[test]
    fn no_after_index_entry_generates_only_the_main_conf() {
        let generated = GeneratedFiles::generate(&test_config()).unwrap();
        assert!(generated_text(&generated, "/etc/btrbk/btrbk.conf").is_some());
        assert!(
            !generated
                .files
                .iter()
                .any(|(p, _)| p.contains("btrbk-state.conf"))
        );
    }

    #[test]
    fn generated_files_manifest() {
        let config = test_config();
        let generated = GeneratedFiles::generate(&config).unwrap();
        let manifest = generated.manifest_content();
        assert!(manifest.contains("/etc/btrbk/btrbk.conf"));
        assert!(manifest.contains("backup-run.sh"));
        assert!(manifest.contains("das-backup.service"));
        assert!(manifest.contains("das-backup.timer"));
        assert!(manifest.contains("das-scrub.service"));
        assert!(manifest.contains("das-scrub.timer"));
        assert!(manifest.contains("das-backup-doctor.service"));
        assert!(manifest.contains("das-backup-doctor.timer"));
        assert!(manifest.contains(UDEV_UDISKS_IGNORE_RULES));
    }

    /// `test_config()` plus a RAID-1 target that carries a `mount_uuid`.
    fn config_with_uuid_target() -> Config {
        let mut config = test_config();
        config.targets.push(Target {
            label: "pair".to_string(),
            serial: "SERIAL11".to_string(),
            serials: vec!["SERIAL11".to_string(), "SERIAL12".to_string()],
            mount_uuid: Some("5eed0016-0000-4000-8000-000000000016".to_string()),
            mount: "/mnt/pair".to_string(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 0,
                monthly: 0,
                daily: 7,
                yearly: 0,
            },
            display_name: String::new(),
            esp_partuuid: None,
        });
        config
    }

    #[test]
    fn render_udev_udisks_ignore_keys_every_target_serial_and_uuid() {
        // bd DAS-Backup-Manager-xqo: one line per configured serial (scoped to
        // the btrfs partition, so a recovery drive's own ESP is not matched),
        // plus one per configured filesystem UUID.
        let result = render_udev_udisks_ignore(&config_with_uuid_target());
        assert!(result.contains("# Generated by btrdasd setup"));
        for serial in ["SERIAL08", "SERIAL11", "SERIAL12"] {
            let line = format!(
                "SUBSYSTEM==\"block\", ENV{{ID_SERIAL_SHORT}}==\"{serial}\", \
                 ENV{{ID_FS_TYPE}}==\"btrfs\", ENV{{UDISKS_IGNORE}}=\"1\"\n"
            );
            assert!(
                result.contains(&line),
                "missing serial rule for {serial}:\n{result}"
            );
        }
        assert!(result.contains(
            "SUBSYSTEM==\"block\", ENV{ID_FS_UUID}==\"5eed0016-0000-4000-8000-000000000016\", \
             ENV{UDISKS_IGNORE}=\"1\"\n"
        ));
        // Three serials + one UUID: exactly four rules, nothing invented for
        // the target that has no mount_uuid.
        assert_eq!(result.matches("ENV{UDISKS_IGNORE}=\"1\"").count(), 4);
    }

    #[test]
    fn render_udev_udisks_ignore_refuses_values_that_would_break_rule_syntax() {
        // A serial or UUID is interpolated into a udev match string. One
        // containing a quote would end the string early and turn the rest of
        // the line into rule syntax, so it gets a comment, never a rule.
        let mut config = test_config();
        config.targets[0].serials = vec!["BAD\", ENV{X}==\"".to_string()];
        config.targets[0].mount_uuid = Some("not a uuid\"".to_string());
        let result = render_udev_udisks_ignore(&config);
        assert_eq!(result.matches("ENV{UDISKS_IGNORE}=\"1\"").count(), 0);
        assert!(!result.contains("BAD"));
        assert_eq!(result.matches("# SKIPPED").count(), 2);
    }

    fn config_with_esp_targets(partuuids: &[Option<&str>]) -> Config {
        let mut config = Config::default();
        for (i, pu) in partuuids.iter().enumerate() {
            config.targets.push(Target {
                label: format!("recovery-{i}"),
                serial: format!("SERIAL0{i}"),
                serials: vec![format!("SERIAL0{i}")],
                mount_uuid: None,
                mount: format!("/mnt/recovery-{i}"),
                role: TargetRole::Mirror,
                retention: Retention {
                    weekly: 0,
                    monthly: 0,
                    daily: 7,
                    yearly: 0,
                },
                display_name: String::new(),
                esp_partuuid: pu.map(str::to_string),
            });
        }
        config
    }

    fn recovery_name_line(partuuid: &str) -> String {
        format!(
            "ACTION!=\"remove\", SUBSYSTEM==\"block\", ENV{{DEVTYPE}}==\"partition\", \
             ENV{{ID_PART_ENTRY_UUID}}==\"{partuuid}\", ENV{{ID_PART_ENTRY_NAME}}=\"RECOVERY\"\n"
        )
    }

    #[test]
    fn render_udev_udisks_ignore_names_each_configured_recovery_esp_recovery() {
        // bd DAS-Backup-Manager-eg4j.18: fwupd skips an ESP whose partition
        // name is RECOVERY; one line per configured esp_partuuid, lower case.
        let config = config_with_esp_targets(&[
            Some("5eed0025-0000-4000-8000-000000000025"),
            Some("5EED0023-0000-4000-8000-000000000023"),
        ]);
        let result = render_udev_udisks_ignore(&config);
        assert!(result.contains(&recovery_name_line("5eed0025-0000-4000-8000-000000000025")));
        assert!(result.contains(&recovery_name_line("5eed0023-0000-4000-8000-000000000023")));
        assert_eq!(result.matches("ID_PART_ENTRY_NAME").count(), 2);
        assert!(result.contains("fu_volume_check_is_recovery"));
        // The existing ignore lines are untouched: two serials, no UUID.
        assert_eq!(result.matches("ENV{UDISKS_IGNORE}=\"1\"").count(), 2);
    }

    #[test]
    fn render_udev_udisks_ignore_names_one_esp_and_none() {
        let one = render_udev_udisks_ignore(&config_with_esp_targets(&[
            Some("5eed0025-0000-4000-8000-000000000025"),
            None,
        ]));
        assert_eq!(one.matches("ID_PART_ENTRY_NAME").count(), 1);
        let none = render_udev_udisks_ignore(&config_with_esp_targets(&[None, None]));
        assert_eq!(none.matches("ID_PART_ENTRY_NAME").count(), 0);
        assert!(!none.contains("fu_volume_check_is_recovery"));
    }

    #[test]
    fn render_udev_udisks_ignore_never_writes_an_invalid_esp_partuuid() {
        let config =
            config_with_esp_targets(&[Some("x\", ENV{X}==\"y"), Some("not-a-partuuid"), Some("")]);
        let result = render_udev_udisks_ignore(&config);
        assert_eq!(result.matches("ID_PART_ENTRY_NAME").count(), 0);
        assert!(!result.contains("ENV{X}"));
        assert_eq!(result.matches("# SKIPPED an esp_partuuid").count(), 3);
    }

    #[test]
    fn udev_safe_value_accepts_serials_and_uuids_only() {
        assert!(is_udev_safe_value("SERIAL04"));
        assert!(is_udev_safe_value("5eed0016-0000-4000-8000-000000000016"));
        assert!(is_udev_safe_value("WD-WX12_A.3"));
        assert!(!is_udev_safe_value(""));
        assert!(!is_udev_safe_value("a\"b"));
        assert!(!is_udev_safe_value("a b"));
        assert!(!is_udev_safe_value("a*"));
        assert!(!is_udev_safe_value("a|b"));
    }

    #[test]
    fn generated_files_carry_the_udev_rule_under_every_init_system() {
        // udisks does not care which init system is running.
        for system in [
            InitSystem::Systemd,
            InitSystem::Sysvinit,
            InitSystem::Openrc,
        ] {
            let mut config = test_config();
            config.init.system = system;
            let generated = GeneratedFiles::generate(&config).unwrap();
            let rule = generated
                .files
                .iter()
                .find(|(p, _)| p == UDEV_UDISKS_IGNORE_RULES)
                .map(|(_, c)| c.as_str());
            assert_eq!(rule, Some(render_udev_udisks_ignore(&config).as_str()));
        }
    }

    #[test]
    fn render_udev_io_scheduler_pins_the_scheduler_on_every_target_serial() {
        // bd DAS-Backup-Manager-su1b.4: one line per configured serial, at every
        // add|change of the whole disk, so a re-plug or power cycle cannot leave
        // the host's generic rule's scheduler on the drive.
        let mut config = config_with_uuid_target();
        config.das.io_scheduler = "mq-deadline".to_string();
        let result = render_udev_io_scheduler(&config).expect("a rule");
        assert!(result.contains("# Generated by btrdasd setup"));
        assert!(result.contains("do not edit"));
        assert!(result.contains("[das].io_scheduler"));
        for serial in ["SERIAL08", "SERIAL11", "SERIAL12"] {
            let line = format!(
                "ACTION==\"add|change\", SUBSYSTEM==\"block\", ENV{{DEVTYPE}}==\"disk\", \
                 ENV{{ID_SERIAL_SHORT}}==\"{serial}\", ATTR{{queue/scheduler}}=\"mq-deadline\", \
                 ATTR{{queue/nr_requests}}=\"64\"\n"
            );
            assert!(result.contains(&line), "missing {serial}:\n{result}");
        }
        assert_eq!(result.matches("ATTR{queue/scheduler}").count(), 3);
        // nr_requests follows the scheduler on the same line: a scheduler
        // switch resets it (bd DAS-Backup-Manager-su1b.8).
        assert_eq!(
            result
                .matches("ATTR{queue/scheduler}=\"mq-deadline\", ATTR{queue/nr_requests}=\"64\"\n")
                .count(),
            3,
            "{result}"
        );
    }

    #[test]
    fn render_udev_io_scheduler_leaves_nr_requests_alone_at_zero() {
        let mut config = config_with_uuid_target();
        config.das.io_scheduler = "mq-deadline".to_string();
        config.das.nr_requests = 0;
        let result = render_udev_io_scheduler(&config).expect("a rule");
        assert!(!result.contains("nr_requests}="), "{result}");
        config.das.nr_requests = 128;
        let result = render_udev_io_scheduler(&config).expect("a rule");
        assert!(
            result.contains(", ATTR{queue/nr_requests}=\"128\"\n"),
            "{result}"
        );
    }

    #[test]
    fn render_udev_io_scheduler_one_serial_and_no_serial() {
        let mut config = test_config();
        config.das.io_scheduler = "bfq".to_string();
        let one = render_udev_io_scheduler(&config).expect("a rule");
        assert_eq!(one.matches("ATTR{queue/scheduler}=\"bfq\"").count(), 1);
        config.targets[0].serials.clear();
        config.targets[0].serial.clear();
        let none = render_udev_io_scheduler(&config).expect("header only");
        assert_eq!(none.matches("ATTR{queue/scheduler}").count(), 0);
    }

    #[test]
    fn render_udev_io_scheduler_lists_a_shared_serial_once_and_skips_unsafe_ones() {
        let mut config = config_with_uuid_target();
        config.das.io_scheduler = "mq-deadline".to_string();
        config.targets[0].serials = vec!["SERIAL11".to_string(), "BAD\", X==\"".to_string()];
        let result = render_udev_io_scheduler(&config).expect("a rule");
        assert_eq!(result.matches("==\"SERIAL11\"").count(), 1);
        assert!(!result.contains("BAD"));
        assert_eq!(result.matches("# SKIPPED").count(), 1);
    }

    #[test]
    fn render_udev_io_scheduler_renders_nothing_for_an_empty_or_unknown_scheduler() {
        let mut config = test_config();
        config.das.io_scheduler = String::new();
        assert_eq!(render_udev_io_scheduler(&config), None);
        config.das.io_scheduler = "mq-deadline\", RUN+=\"x".to_string();
        assert_eq!(render_udev_io_scheduler(&config), None);
    }

    #[test]
    fn generate_carries_the_io_scheduler_rule_only_while_a_scheduler_is_configured() {
        for system in [
            InitSystem::Systemd,
            InitSystem::Sysvinit,
            InitSystem::Openrc,
        ] {
            let mut config = test_config();
            config.init.system = system;
            config.das.io_scheduler = "mq-deadline".to_string();
            let generated = GeneratedFiles::generate(&config).unwrap();
            let rule = generated
                .files
                .iter()
                .find(|(p, _)| p == UDEV_IO_SCHEDULER_RULES)
                .map(|(_, c)| c.clone());
            assert!(rule.is_some());
            assert_eq!(rule, render_udev_io_scheduler(&config));
            config.das.io_scheduler = String::new();
            let off = GeneratedFiles::generate(&config).unwrap();
            assert!(!off.files.iter().any(|(p, _)| p == UDEV_IO_SCHEDULER_RULES));
        }
    }

    #[test]
    fn generate_refuses_a_scheduler_outside_the_safe_set() {
        let mut config = test_config();
        config.das.io_scheduler = "cfq".to_string();
        let why = GeneratedFiles::generate(&config).err().expect("refused");
        assert!(why.contains("io_scheduler"), "{why}");
    }

    #[test]
    fn generate_refuses_an_nr_requests_the_kernel_would_change() {
        let mut config = test_config();
        config.das.nr_requests = 2;
        let why = GeneratedFiles::generate(&config).err().expect("refused");
        assert!(why.contains("nr_requests 2"), "{why}");
    }
}
