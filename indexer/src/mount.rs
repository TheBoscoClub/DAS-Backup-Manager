//! Auto-mount and unmount DAS backup targets.
//!
//! Provides [`ensure_targets_mounted`] which finds each target by its
//! `mount_uuid` or its drive serials, mounts any that are not already
//! mounted, and returns a
//! [`MountGuard`] whose [`Drop`] implementation unmounts only the targets that
//! *this* call mounted — never interfering with mounts managed by the bash
//! scripts or by the user.

use std::fmt;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::panic::RefUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, Target, TargetRole};
use crate::health;
use crate::maintenance::MaintenanceHeld;
use crate::progress::ProgressCallback;
use crate::raid_guard;

/// Errors that can occur during the mount lifecycle.
#[derive(Debug)]
pub enum MountError {
    /// No DAS drives were detected at all (enclosure likely off or disconnected).
    NoDrivesFound,
    /// A specific target's serial was not found in `/dev/disk/by-id/`.
    DriveNotFound { label: String, serial: String },
    /// The expected partition device does not exist.
    PartitionNotFound { label: String, partition: String },
    /// `mount(8)` returned a non-zero exit code.
    MountFailed { label: String, detail: String },
    /// `mkdir -p` failed for the mount point directory.
    MkdirFailed { label: String, path: String },
    /// An earlier job left the target mounted, and neither the plain unmount
    /// (retried) nor `umount -f` released it (bd DAS-Backup-Manager-jgm).
    LeftoverMounted {
        label: String,
        mount: String,
        detail: String,
    },
    /// Something is mounted at the target's path that cannot be shown to be
    /// the target. Never unmounted: it is not this job's.
    ForeignMount {
        label: String,
        mount: String,
        detail: String,
    },
    /// A RAID-1 target's guarded mount refused (bd DAS-Backup-Manager-su1b.10):
    /// a split brain, an unreadable record, or the kernel missing a member
    /// while every drive is attached. Never a target merely skipped.
    RaidRefused { label: String, detail: String },
    /// Every target was skipped because its clearing refused (a writer, a
    /// maintenance operation…): the notes say each and why (bd su1b.31).
    AllTargetsRefused { notes: Vec<ClearNote> },
}

impl fmt::Display for MountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDrivesFound => write!(f, "No DAS drives found — is the enclosure powered on?"),
            Self::AllTargetsRefused { notes } => write!(
                f,
                "No target could be used: {}",
                notes
                    .iter()
                    .map(|n| format!("'{}': {}", n.label, n.detail))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Self::DriveNotFound { label, serial } => {
                write!(
                    f,
                    "Target '{label}': drive with serial '{serial}' not found"
                )
            }
            Self::PartitionNotFound { label, partition } => {
                write!(
                    f,
                    "Target '{label}': partition '{partition}' does not exist"
                )
            }
            Self::MountFailed { label, detail } => {
                write!(f, "Target '{label}': mount failed: {detail}")
            }
            Self::MkdirFailed { label, path } => {
                write!(f, "Target '{label}': mkdir -p '{path}' failed")
            }
            Self::LeftoverMounted {
                label,
                mount,
                detail,
            } => write!(
                f,
                "Target '{label}': {mount} was left mounted by an earlier job and cannot be \
                 unmounted ({detail}) — refusing to run until it is"
            ),
            Self::ForeignMount {
                label,
                mount,
                detail,
            } => write!(
                f,
                "Target '{label}': {mount} is mounted but is not this target ({detail}) — \
                 refusing to run; it is not this job's to unmount"
            ),
            Self::RaidRefused { label, detail } => {
                write!(f, "Target '{label}': RAID-1 mount refused: {detail}")
            }
        }
    }
}

impl std::error::Error for MountError {}

/// Determine the partition device path for a target based on its role.
///
/// - **Primary** targets use the first partition (`{dev}1`) — whole-disk BTRFS
///   with a single partition table entry.
/// - **Mirror** targets use the second partition (`{dev}2`) —
///   partition 1 is the ESP, partition 2 is the BTRFS data area.
pub fn partition_device(dev: &str, role: &TargetRole) -> String {
    match role {
        TargetRole::Primary => format!("{dev}1"),
        TargetRole::Mirror => format!("{dev}2"),
    }
}

/// The one place this module spawns a process.
///
/// `mount(8)`, `umount(8)` and `findmnt(8)` all act on, or report on, the
/// running system, so the decisions built around them — what counts as
/// mounted, what a guard owes an unmount, when a failure must leave a target
/// unavailable — cannot be exercised against the real tools without mounting
/// real drives. Everything below builds its `Command` as before and hands it
/// here to be run; the tests hand it to a scripted runner instead.
///
/// The supertraits keep [`MountGuard`], which carries a runner so that `Drop`
/// can unmount, exactly as thread- and unwind-safe as it was when it held
/// nothing but a `Vec<String>`.
trait CommandRunner: Send + Sync + RefUnwindSafe {
    /// Run to completion with inherited stdio.
    fn status(&self, cmd: &mut Command) -> io::Result<ExitStatus>;
    /// Run to completion, capturing stdout and stderr.
    fn output(&self, cmd: &mut Command) -> io::Result<Output>;
    /// Wait between two attempts of a command (an unmount retry). Here so
    /// that tests, which never run the real commands, never really wait.
    fn pause(&self, how_long: Duration);
}

/// Runs commands for real.
struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn status(&self, cmd: &mut Command) -> io::Result<ExitStatus> {
        cmd.status()
    }

    fn output(&self, cmd: &mut Command) -> io::Result<Output> {
        cmd.output()
    }

    fn pause(&self, how_long: Duration) {
        std::thread::sleep(how_long);
    }
}

/// The questions this module asks about the host's mount table and
/// `/dev/disk/by-id`. Held as borrowed functions rather than called directly
/// so the mount logic can be run against a described host; production code
/// only ever uses [`HOST_PROBES`].
/// `health::member_drive_serial`'s shape: partition path, configured serials.
type SerialOfPartition<'a> = dyn Fn(&str, &[String]) -> Result<String, String> + 'a;

struct MountProbes<'a> {
    is_mountpoint: &'a dyn Fn(&Path) -> bool,
    find_mount_for_device: &'a dyn Fn(&str, &TargetRole) -> Option<String>,
    device_from_serial: &'a dyn Fn(&str) -> Option<String>,
    /// A device holding the filesystem with this UUID (`blkid -U`).
    device_for_uuid: &'a dyn Fn(&str) -> Option<String>,
    /// The devids the kernel marks missing for a mounted filesystem
    /// (`raid_guard::missing_devids`).
    missing_devids: &'a dyn Fn(&str) -> Result<Vec<u64>, String>,
    /// The serial of the drive a member partition sits on, given the
    /// target's configured serials (`health::member_drive_serial`).
    serial_of_partition: &'a SerialOfPartition<'a>,
}

const HOST_PROBES: MountProbes<'static> = MountProbes {
    is_mountpoint: &health::is_mountpoint,
    find_mount_for_device: &health::find_mount_for_device,
    device_from_serial: &health::device_from_serial,
    device_for_uuid: &health::device_for_fs_uuid,
    missing_devids: &raid_guard::missing_devids,
    serial_of_partition: &health::member_drive_serial,
};

/// The words that name the processes a clearing stopped, in the job's log
/// and in `CLEAR TARGET: OK` (which `backup-run.sh` reads, bd su1b.12).
pub const STOPPED: &str = "stopped what held it";

/// What a target's clearing did to another program's processes, kept for the
/// job's report (bd DAS-Backup-Manager-su1b.31; `backup-run.sh`'s
/// `CLEAR_NOTES`): the log alone is not the operator's stop policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearOutcome {
    /// Processes were stopped (and the mount taken down): the row is WARN.
    Stopped,
    /// A dry run: what it would have stopped, nothing was: WARN.
    WouldStop,
    /// The target was skipped (a writer, a protected process, maintenance,
    /// a mount that would not come down): the row is FAIL.
    Refused,
}

/// One target's clearing, as a report row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearNote {
    pub label: String,
    pub outcome: ClearOutcome,
    pub detail: String,
}

impl ClearNote {
    /// Whether this note fails the run (a target skipped).
    pub fn is_fail(&self) -> bool {
        self.outcome == ClearOutcome::Refused
    }

    /// The `BACKUP OPERATIONS` row, as `backup-run.sh` writes the target's.
    pub fn row(&self) -> String {
        let (status, text) = match self.outcome {
            ClearOutcome::Stopped => ("WARN", format!("before the mount: {}", self.detail)),
            ClearOutcome::WouldStop => ("WARN", format!("dry run: {}", self.detail)),
            ClearOutcome::Refused => ("FAIL", format!("not used this run: {}", self.detail)),
        };
        format!(
            "  {:<21} {status}  ({text})",
            format!("Target {}", self.label)
        )
    }
}

/// What clearing a target's other mounts and their holders reads and does
/// (bd DAS-Backup-Manager-su1b.12): the mount table, the process table, the
/// signals, and the kernel's list of mounted btrfs filesystems. Production
/// only ever uses [`host_clear_site`].
struct ClearSite<'a> {
    mountinfo: &'a dyn Fn() -> Result<String, String>,
    proc: crate::holders::ProcSite,
    signaller: &'a dyn crate::holders::Signaller,
    /// This process: never signalled, nor any ancestor of it.
    self_pid: i32,
    /// A dry run: holders and mounts are said ("would stop", "would
    /// unmount"), nothing is signalled or unmounted (bd su1b.12 review).
    report_only: bool,
    /// The job may stop readers: only one that writes under the target
    /// (operator 2026-11-25, [`raid_guard::Job::stops_readers`]). `false` →
    /// any holder refuses the target, named.
    stop_readers: bool,
    /// `/sys/fs/btrfs`: `<fsid>` is there exactly while the filesystem is
    /// mounted in some mount namespace (proven on a VM, kernel 7.3: absent
    /// when only registered by a scan or after its last unmount; present
    /// while mounted only in another namespace, and after a lazy unmount
    /// while something still holds it).
    sysfs_btrfs: PathBuf,
    /// The block device number (`major:minor`) of a partition path; `Err`
    /// when the path is not a block device (bd DAS-Backup-Manager-eg4j.19).
    block_dev: &'a dyn Fn(&str) -> Result<String, String>,
}

/// `st_rdev` of a block device, as mountinfo writes it: never of anything
/// that is not one.
fn host_block_dev(path: &str) -> Result<String, String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let m = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if !m.file_type().is_block_device() {
        return Err(format!("{path} is not a block device"));
    }
    let r = m.rdev();
    Ok(format!("{}:{}", libc::major(r), libc::minor(r)))
}

fn read_self_mountinfo() -> Result<String, String> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("/proc/self/mountinfo: {e}"))
}

fn host_clear_site(report_only: bool, stop_readers: bool) -> ClearSite<'static> {
    ClearSite {
        report_only,
        stop_readers,
        mountinfo: &read_self_mountinfo,
        proc: crate::holders::ProcSite::host(),
        signaller: &crate::holders::SystemSignaller,
        self_pid: std::process::id() as i32,
        sysfs_btrfs: PathBuf::from("/sys/fs/btrfs"),
        block_dev: &host_block_dev,
    }
}

/// The guarded mount's host on this machine: this module's runner and
/// probes (bd DAS-Backup-Manager-su1b.10).
struct SystemMembers<'a> {
    runner: &'a dyn CommandRunner,
    probes: &'a MountProbes<'a>,
}

impl SystemMembers<'_> {
    fn run(&self, cmd: &mut Command) -> Result<Output, String> {
        let what = format!("{cmd:?}");
        self.runner
            .output(cmd)
            .map_err(|e| format!("{what} could not be run: {e}"))
    }
}

fn failed(what: &str, out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    format!(
        "{what} exited with code {}{}",
        out.status.code().unwrap_or(-1),
        if stderr.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", stderr.trim())
        }
    )
}

impl raid_guard::MemberHost for SystemMembers<'_> {
    fn members(&self, fsid: &str) -> Result<Vec<String>, String> {
        // `-c /dev/null`: read the disks now, never a cache.
        let out = self.run(Command::new("blkid").args([
            "-c",
            "/dev/null",
            "-t",
            &format!("UUID={fsid}"),
            "-o",
            "device",
        ]))?;
        match out.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()),
            // blkid's "nothing matched".
            Some(2) => Ok(Vec::new()),
            _ => Err(failed("blkid", &out)),
        }
    }

    fn scan(&self, path: &str) -> Result<(), String> {
        let out = self.run(Command::new("btrfs").args(["device", "scan", path]))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(failed("btrfs device scan", &out))
        }
    }

    fn forget(&self, path: &str) -> Result<(), String> {
        let out = self.run(Command::new("btrfs").args(["device", "scan", "--forget", path]))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(failed("btrfs device scan --forget", &out))
        }
    }

    fn mount_device(&self, opts: &str, device: &str, at: &str) -> Result<(), String> {
        let out = self.run(Command::new("mount").args(["-t", "btrfs", "-o", opts, device, at]))?;
        if out.status.success() {
            return Ok(());
        }
        let why = failed("mount", &out);
        if (self.probes.is_mountpoint)(Path::new(at))
            && let Err(u) = umount_retrying(self.runner, at, |_| {})
        {
            return Err(format!(
                "{why}; it is mounted anyway and will not unmount: {u}"
            ));
        }
        Err(why)
    }

    fn temp_mountpoint(&self) -> Result<String, String> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let at = format!(
            "/run/btrdasd-raid-probe.{}.{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&at)
            .map_err(|e| format!("cannot create {at}: {e}"))?;
        Ok(at)
    }

    fn remove_mountpoint(&self, at: &str) -> Result<(), String> {
        std::fs::remove_dir(at).map_err(|e| format!("cannot remove {at}: {e}"))
    }

    fn fsid_mounted(&self, fsid: &str) -> Result<bool, String> {
        let out = self.run(Command::new("findmnt").args([
            "-n",
            "-o",
            "TARGET",
            "-S",
            &format!("UUID={fsid}"),
        ]))?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed("findmnt", &out)),
        }
    }

    fn read_super(&self, path: &str) -> Result<String, String> {
        let out = self.run(Command::new("btrfs").args(["inspect-internal", "dump-super", path]))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(failed("btrfs inspect-internal dump-super", &out))
        }
    }

    fn serial_attached(&self, serial: &str) -> bool {
        (self.probes.device_from_serial)(serial).is_some()
    }

    fn member_serial(&self, path: &str, configured: &[String]) -> Result<String, String> {
        (self.probes.serial_of_partition)(path, configured)
    }

    fn mount(&self, opts: &str, fsid: &str, mount: &str) -> Result<(), String> {
        let mut cmd = Command::new("mount");
        cmd.args(["-t", "btrfs"]);
        if !opts.is_empty() {
            cmd.args(["-o", opts]);
        }
        cmd.arg(format!("UUID={fsid}")).arg(mount);
        let out = self.run(&mut cmd)?;
        if out.status.success() {
            return Ok(());
        }
        let why = failed("mount", &out);
        // A mount helper that failed after the kernel mounted (bd 1fpw): take
        // it back down so the guard's next step starts from nothing.
        if (self.probes.is_mountpoint)(Path::new(mount))
            && let Err(u) = umount_retrying(self.runner, mount, |_| {})
        {
            return Err(format!(
                "{why}; it is mounted anyway and will not unmount: {u}"
            ));
        }
        Err(why)
    }

    fn umount(&self, mount: &str) -> Result<(), String> {
        umount_retrying(self.runner, mount, |_| {})
    }

    fn missing_devids(&self, fsid: &str) -> Result<Vec<u64>, String> {
        (self.probes.missing_devids)(fsid)
    }

    fn devinfo(&self, fsid: &str) -> Result<Vec<(u64, bool)>, String> {
        raid_guard::devinfo_under(Path::new("/sys/fs/btrfs"), fsid)
    }

    fn opened_devices(&self, fsid: &str) -> Result<Vec<String>, String> {
        raid_guard::opened_devices_under(Path::new("/sys/fs/btrfs"), fsid)
    }

    fn remount_rw(&self, mount: &str) -> Result<(), String> {
        let out = self.run(Command::new("mount").args(["-o", "remount,rw", mount]))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(failed("mount -o remount,rw", &out))
        }
    }

    fn read_witness(&self, mount: &str, fsid: &str) -> raid_guard::Copy {
        raid_guard::read_copy(&raid_guard::witness_path(mount), fsid)
    }

    fn write_witness(&self, mount: &str, record: &raid_guard::Record) -> Result<(), String> {
        raid_guard::write_copy(&raid_guard::witness_path(mount), record, false)
    }

    fn pause(&self, how_long: Duration) {
        self.runner.pause(how_long);
    }

    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// The guarded mount of one RAID-1 target (`raid_guard::is_guarded`) on this
/// host, at its configured path, which must exist and not be a mount point.
/// For a job that mounts a target by its own code (the scrub engine,
/// `btrdasd target mount` for `backup-run.sh`); the caller owns the unmount.
/// `held` proves the maintenance lock, as for [`ensure_targets_mounted`].
pub fn mount_guarded_target(
    config: &Config,
    target: &Target,
    intent: raid_guard::Intent,
    progress: &dyn ProgressCallback,
    _held: &MaintenanceHeld,
) -> Result<raid_guard::Outcome, raid_guard::Failure> {
    mount_guarded_with(
        config,
        target,
        intent,
        &raid_guard::record_dir(),
        progress,
        &HOST_PROBES,
        &SystemRunner,
    )
}

/// [`mount_guarded_target`] for the scrub engine (bd
/// DAS-Backup-Manager-eg4j.20): another program's mount of the filesystem
/// is cleared first by the stop policy, as a job that writes (a scrub
/// repairs), exactly as every other mounting path does (bd eg4j.19). What
/// cannot be cleared, or any such mount under a handed-down hold, is the
/// `Refused`: the filesystem is not scrubbed, and the mount is never made
/// beside the other program's.
pub fn mount_guarded_target_clearing(
    config: &Config,
    target: &Target,
    intent: raid_guard::Intent,
    progress: &dyn ProgressCallback,
    held: &MaintenanceHeld,
) -> Result<raid_guard::Outcome, raid_guard::Failure> {
    mount_guarded_clearing_with(
        config,
        target,
        intent,
        &raid_guard::record_dir(),
        progress,
        &HOST_PROBES,
        &SystemRunner,
        &host_clear_site(false, true),
        held.is_delegated(),
    )
}

#[allow(clippy::too_many_arguments)] // one host, one target, one job
fn mount_guarded_clearing_with(
    config: &Config,
    target: &Target,
    intent: raid_guard::Intent,
    record_dir: &Path,
    progress: &dyn ProgressCallback,
    probes: &MountProbes<'_>,
    runner: &dyn CommandRunner,
    clear: &ClearSite<'_>,
    delegated: bool,
) -> Result<raid_guard::Outcome, raid_guard::Failure> {
    let fsid = target.mount_uuid.as_deref().unwrap_or_default();
    clear_elsewhere_with(
        runner,
        probes,
        clear,
        &target.label,
        fsid,
        &target.mount,
        delegated,
        progress,
    )
    .map_err(|why| raid_guard::Failure::Refused {
        why: format!("another program's mount of {fsid} cannot be cleared: {why}"),
        left_mounted: false,
    })?;
    mount_guarded_with(config, target, intent, record_dir, progress, probes, runner)
}

fn mount_guarded_with(
    config: &Config,
    target: &Target,
    intent: raid_guard::Intent,
    record_dir: &Path,
    progress: &dyn ProgressCallback,
    probes: &MountProbes<'_>,
    runner: &dyn CommandRunner,
) -> Result<raid_guard::Outcome, raid_guard::Failure> {
    let Some(fsid) = target.mount_uuid.as_deref().filter(|u| !u.is_empty()) else {
        return Err(raid_guard::Failure::MountFailed(
            "no mount_uuid: not a RAID-1 target".into(),
        ));
    };
    let serials = target.effective_serials();
    let host = SystemMembers { runner, probes };
    raid_guard::mount_guarded(
        &host,
        &raid_guard::Plan {
            label: &target.label,
            fsid,
            mount: &target.mount,
            serials: &serials,
            mount_opts: &config.das.mount_opts,
            wait: Duration::from_secs(config.das.member_wait_secs.into()),
            record_dir,
            intent,
        },
        progress,
    )
}

/// The end-of-job check of `target`, mounted at its path by this process's
/// own code (the scrub engine, `btrdasd backup raid-check` for
/// `backup-run.sh`), before it is unmounted (bd DAS-Backup-Manager-su1b.14).
pub fn raid_end_check(config: &Config, target: &Target) -> raid_guard::EndCheck {
    let serials = target.effective_serials();
    let record_dir = raid_guard::record_dir();
    let host = SystemMembers {
        runner: &SystemRunner,
        probes: &HOST_PROBES,
    };
    raid_guard::check_before_unmount(
        &host,
        &raid_guard::Plan {
            label: &target.label,
            fsid: target.mount_uuid.as_deref().unwrap_or_default(),
            mount: &target.mount,
            serials: &serials,
            mount_opts: &config.das.mount_opts,
            wait: Duration::ZERO,
            record_dir: &record_dir,
            intent: raid_guard::Job::Backup.intent(),
        },
    )
}

/// What `btrdasd raid accept` did, and what could not be logged.
#[derive(Debug)]
pub struct RaidAccepted {
    pub accepted: raid_guard::Accepted,
    /// The journal line or the history row that could not be written: the
    /// record is rewritten all the same (that is the act).
    pub warnings: Vec<String>,
}

/// The RAID-1 target labelled `label`, its mount point ready (created when
/// missing, never already a mount point), for `raid accept`. Returns whether
/// the directory was created here.
fn accept_target<'a>(
    config: &'a Config,
    label: &str,
    probes: &MountProbes<'_>,
) -> Result<(&'a Target, bool), String> {
    let target = config
        .targets
        .iter()
        .find(|t| t.label == label)
        .ok_or_else(|| format!("no [[target]] labelled '{label}'"))?;
    if !raid_guard::is_guarded(target) {
        return Err(format!(
            "target '{label}' is not a RAID-1 target (mount_uuid and two or more serials)"
        ));
    }
    let mount = Path::new(&target.mount);
    if (probes.is_mountpoint)(mount) {
        return Err(format!(
            "{} is mounted: nothing can be accepted while a job uses the target",
            target.mount
        ));
    }
    let created = !mount.exists();
    if created {
        std::fs::create_dir_all(mount)
            .map_err(|e| format!("cannot create {}: {e}", target.mount))?;
    }
    Ok((target, created))
}

fn accept_plan<'a>(
    config: &'a Config,
    target: &'a Target,
    serials: &'a [String],
    record_dir: &'a Path,
) -> raid_guard::Plan<'a> {
    raid_guard::Plan {
        label: &target.label,
        fsid: target.mount_uuid.as_deref().unwrap_or_default(),
        mount: &target.mount,
        serials,
        mount_opts: &config.das.mount_opts,
        wait: Duration::ZERO,
        record_dir,
        intent: raid_guard::Job::RaidAccept.intent(),
    }
}

/// What `btrdasd raid accept <label>` would show, without asking or writing:
/// the explanation when the guard refuses the target for split brain now,
/// else why there is nothing to accept. It probe-mounts the target
/// read-only, so `held` proves the maintenance lock.
pub fn raid_accept_preview(
    config: &Config,
    label: &str,
    db: &Path,
    progress: &dyn ProgressCallback,
    _held: &MaintenanceHeld,
) -> Result<String, String> {
    let (target, created) = accept_target(config, label, &HOST_PROBES)?;
    let serials = target.effective_serials();
    let record_dir = raid_guard::record_dir();
    let plan = accept_plan(config, target, &serials, &record_dir);
    let host = SystemMembers {
        runner: &SystemRunner,
        probes: &HOST_PROBES,
    };
    let result = raid_guard::preview(
        &host,
        &plan,
        &|from, to| crate::db::count_backup_runs_between_read_only(db, from, to),
        progress,
    );
    if created {
        remove_created_mount_point(&format!("Target '{label}'"), &target.mount, progress);
    }
    result
}

/// `btrdasd raid accept <label>` on this host (bd DAS-Backup-Manager-su1b.11):
/// [`raid_guard::accept`], then one line to the journal (`logger -t btrdasd
/// -p user.warning`) and one row in the database's `raid_accepted`. The
/// caller holds the backup singleton (`_singleton`) and the maintenance lock
/// (`_held`), as a configuration edit and a mount do. `confirm` is shown the
/// explanation and returns what was typed; an `Err` (no terminal, no typed
/// text) accepts nothing. `ask` is the terminal for the case where every
/// member is present and their generations differ (bd su1b.21): handed
/// `(text, prompt)` it returns what was typed (the label, then the serial of
/// the member to keep). `None` (the helper: no picker) refuses that case,
/// saying to use the command line.
pub fn raid_accept(
    config: &Config,
    label: &str,
    db: &Path,
    terminal: raid_guard::Terminal<'_>,
    progress: &dyn ProgressCallback,
    _singleton: &crate::backup::SingletonHeld,
    _held: &MaintenanceHeld,
) -> Result<RaidAccepted, String> {
    let (target, created) = accept_target(config, label, &HOST_PROBES)?;
    let serials = target.effective_serials();
    let record_dir = raid_guard::record_dir();
    let plan = accept_plan(config, target, &serials, &record_dir);
    let host = SystemMembers {
        runner: &SystemRunner,
        probes: &HOST_PROBES,
    };
    let runs = |from, to| crate::db::count_backup_runs_between_read_only(db, from, to);
    let raid_guard::Terminal { confirm, ask } = terminal;
    let result = raid_guard::accept_any(&host, &plan, &runs, confirm, ask, progress);
    if created {
        remove_created_mount_point(&format!("Target '{label}'"), &target.mount, progress);
    }
    let accepted = result?;
    let mut warnings = Vec::new();
    let list = |ms: &[raid_guard::PeriodMember]| {
        ms.iter()
            .map(|m| format!("{} {}", m.devid, m.dev_uuid))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let given_up: Vec<raid_guard::PeriodMember> = accepted
        .after
        .rejected
        .iter()
        .filter(|r| {
            !accepted
                .before
                .rejected
                .iter()
                .any(|o| o.dev_uuid == r.dev_uuid)
        })
        .cloned()
        .collect();
    let line = if accepted.diverged {
        format!(
            "raid accept: target '{label}' (filesystem {}): every member was present with \
             differing superblock generations and no record; devid(s) {} kept by the \
             operator as the history from {}; devid(s) {} given up with whatever only they \
             hold",
            plan.fsid,
            list(&accepted.after.members),
            raid_guard::when(accepted.after.since),
            list(&given_up),
        )
    } else {
        format!(
            "raid accept: target '{label}' (filesystem {}): devid(s) {} accepted as the history \
             from {}; devid(s) {} given up, with the degraded period {} to {} that only they hold",
            plan.fsid,
            list(&accepted.after.members),
            raid_guard::when(accepted.after.since),
            list(&given_up),
            raid_guard::when(accepted.before.since),
            raid_guard::when(accepted.before.last),
        )
    };
    match SystemRunner.status(
        Command::new("logger")
            .args(["-t", "btrdasd", "-p", "user.warning", "--"])
            .arg(&line),
    ) {
        Ok(s) if s.success() => {}
        Ok(s) => warnings.push(format!(
            "the journal line was not written ({})",
            exit_why("logger", s)
        )),
        Err(e) => warnings.push(format!("the journal line was not written (logger: {e})")),
    }
    let row = crate::db::RaidAcceptance {
        accepted_at: accepted.after.since,
        target: label.to_string(),
        fsid: plan.fsid.to_string(),
        accepted: list(&accepted.after.members),
        rejected: list(&given_up),
        period_since: accepted.before.since,
        period_last: accepted.before.last,
    };
    match db.to_str().map(crate::db::Database::open) {
        Some(Ok(database)) => {
            if let Err(e) = database.record_raid_accept(&row) {
                warnings.push(format!("the history row was not written ({e})"));
            }
        }
        Some(Err(e)) => warnings.push(format!(
            "the history row was not written ({}: {e})",
            db.display()
        )),
        None => warnings.push(format!(
            "the history row was not written ({} is not UTF-8)",
            db.display()
        )),
    }
    Ok(RaidAccepted { accepted, warnings })
}

/// RAII guard that unmounts targets on drop.
///
/// Only the mount points that *this* guard mounted are tracked. Pre-existing
/// mounts (from bash scripts, manual mounts, or a previous guard) are never
/// touched.
pub struct MountGuard {
    newly_mounted: Vec<String>,
    /// The RAID-1 targets this guard mounted read-write through the guard:
    /// each is checked for a member that dropped during the job before it is
    /// unmounted (bd DAS-Backup-Manager-su1b.14).
    guarded: Vec<GuardedMount>,
    runner: Arc<dyn CommandRunner>,
    /// What this guard holds, for the progress stage it reports on release.
    held_kind: &'static str,
    /// Each mirror target's ESP directory under [`ESP_MOUNT_ROOT`] and the
    /// `PARTUUID=` spec of its `esp_partuuid`: a recovery-OS reading that
    /// could not take its ESP down leaves it there, and it holds the drive
    /// as the target does. Released before the targets, and only when
    /// `findmnt` shows that partition mounted at that directory (bd
    /// DAS-Backup-Manager-s5jv.2; `backup-run.sh`'s `unmount_all`, s5jv.1).
    esps: Vec<(String, String)>,
    /// What clearing other programs' processes did, per target, for the
    /// report (bd DAS-Backup-Manager-su1b.31).
    clear_notes: Vec<ClearNote>,
}

/// How many times an unmount is tried before the mount point is reported as
/// still mounted. A target is commonly "busy" for a moment after a job ends
/// — a dry run mounts and unmounts it seconds apart — and the busy state
/// was observed to clear by itself (bd DAS-Backup-Manager-5oc).
pub const UMOUNT_ATTEMPTS: u32 = 5;

/// The wait between two unmount attempts.
pub const UMOUNT_RETRY_PAUSE: Duration = Duration::from_secs(2);

/// The error a job reports for mount points it could not release, or `None`
/// when everything was released. Every caller of [`MountGuard::unmount`]
/// turns a non-empty list into a failure with this text, so the operator
/// reads the same words wherever it happens.
pub fn still_mounted_error(still_mounted: &[String]) -> Option<String> {
    (!still_mounted.is_empty()).then(|| format!("still mounted: {}", still_mounted.join(", ")))
}

/// `Err("still mounted: …")` when anything is left mounted — for a caller
/// whose result is a `Result`.
pub fn require_released(still_mounted: &[String]) -> Result<(), String> {
    still_mounted_error(still_mounted).map_or(Ok(()), Err)
}

/// A job's result, failed if anything it mounted is still mounted. A job
/// that had already failed keeps its own error, with the mount points
/// appended.
pub fn fail_if_still_mounted<T>(
    result: Result<T, String>,
    still_mounted: &[String],
) -> Result<T, String> {
    match (result, still_mounted_error(still_mounted)) {
        (result, None) => result,
        (Ok(_), Some(e)) => Err(e),
        (Err(why), Some(e)) => Err(format!("{why}; {e}")),
    }
}

/// `umount` one mount point, up to [`UMOUNT_ATTEMPTS`] times with
/// [`UMOUNT_RETRY_PAUSE`] between attempts. `Err` carries why the last
/// attempt failed; `retrying` is told about each earlier one.
fn umount_retrying(
    runner: &dyn CommandRunner,
    mount_point: &str,
    mut retrying: impl FnMut(&str),
) -> Result<(), String> {
    let mut why = String::new();
    for attempt in 1..=UMOUNT_ATTEMPTS {
        if attempt > 1 {
            retrying(&format!(
                "{mount_point}: {why} (attempt {} of {UMOUNT_ATTEMPTS}) — retrying",
                attempt - 1
            ));
            runner.pause(UMOUNT_RETRY_PAUSE);
        }
        why = match runner.status(Command::new("umount").arg(mount_point)) {
            Ok(s) if s.success() => return Ok(()),
            Ok(s) => exit_why("umount", s),
            Err(e) => format!("umount failed to run: {e}"),
        };
    }
    Err(why)
}

fn exit_why(what: &str, status: ExitStatus) -> String {
    format!("{what} exited with code {}", status.code().unwrap_or(-1))
}

/// Take down a mount an earlier job left behind (bd DAS-Backup-Manager-jgm,
/// operator 2026-11-09): the plain unmount with its retries; still busy and
/// `clear` given, what holds it gets the stop policy (bd
/// DAS-Backup-Manager-su1b.12: a writer refuses here, never forced) and, when
/// that stopped something, the plain tries again; then one `umount -f`.
/// `Err` says why it stayed; the caller then refuses to run.
///
/// Never `umount -l`: a lazy unmount takes the path out of the mount table
/// while the filesystem stays live and its device held, which is the state
/// the "safe to disconnect" check and a recovery-drive VM session exist to
/// catch. On a busy local btrfs `-f` is refused like the plain unmount
/// (measured on a loop device, 2026-11-09): the force step is what the
/// operator asked for, not a stronger tool.
fn take_down_leftover_with(
    runner: &dyn CommandRunner,
    mount_point: &str,
    progress: &dyn ProgressCallback,
    clear: Option<(&ClearSite<'_>, &str, Option<&str>)>,
) -> Result<Vec<String>, String> {
    let mut why = match umount_retrying(runner, mount_point, |msg| {
        progress.on_log(crate::progress::LogLevel::Warning, msg);
    }) {
        Ok(()) => return Ok(Vec::new()),
        Err(why) => why,
    };
    // What holds it busy (bd DAS-Backup-Manager-su1b.12): the operator's
    // stop policy, then the plain unmount again — only when something was
    // stopped; a writer refuses here, never forced.
    let mut stopped: Vec<String> = Vec::new();
    if let Some((site, who, fsid)) = clear {
        progress.on_log(
            crate::progress::LogLevel::Warning,
            &format!(
                "{mount_point}: {why} after {UMOUNT_ATTEMPTS} attempts — looking for what holds it"
            ),
        );
        match (site.mountinfo)() {
            Ok(text) => {
                let at: Vec<crate::holders::MountEntry> = crate::holders::parse_mountinfo(&text)
                    .into_iter()
                    .filter(|m| m.mount_point == mount_point)
                    .collect();
                if let Err(busy) = maintenance_on(runner, site, fsid, mount_point) {
                    return Err(format!("{why} after {UMOUNT_ATTEMPTS} attempts; {busy}"));
                }
                match clear_logged(site, runner, who, &at, progress) {
                    Ok(sent) if sent.is_empty() => {}
                    Ok(sent) => {
                        progress.on_log(
                            crate::progress::LogLevel::Warning,
                            &format!(
                                "Target '{who}': {}",
                                crate::holders::row(
                                    "action",
                                    &format!("{mount_point}: {STOPPED}: {}", sent.join(", "))
                                )
                            ),
                        );
                        stopped = sent.clone();
                        match umount_retrying(runner, mount_point, |msg| {
                            progress.on_log(crate::progress::LogLevel::Warning, msg);
                        }) {
                            Ok(()) => return Ok(sent),
                            Err(w) => why = w,
                        }
                    }
                    Err(refused) => {
                        return Err(format!("{why} after {UMOUNT_ATTEMPTS} attempts; {refused}"));
                    }
                }
            }
            Err(e) => progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!("{mount_point}: what holds it cannot be read ({e})"),
            ),
        }
    }
    progress.on_log(
        crate::progress::LogLevel::Warning,
        &format!("{mount_point}: {why} after {UMOUNT_ATTEMPTS} attempts — retrying with umount -f"),
    );
    let forced = match runner.status(Command::new("umount").arg("-f").arg(mount_point)) {
        Ok(s) if s.success() => return Ok(stopped),
        Ok(s) => exit_why("umount -f", s),
        Err(e) => format!("umount -f failed to run: {e}"),
    };
    Err(format!("{why} after {UMOUNT_ATTEMPTS} attempts; {forced}"))
}

/// What `btrdasd backup clear-target` found (bd DAS-Backup-Manager-su1b.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearTarget {
    /// Nothing holds it any more (or nothing did): the detail says which.
    Cleared(String),
    /// A writer, a protected process, a mount that is not the target, or one
    /// that will not come down: the target is not to be used this run.
    Refused(String),
    /// The mount table or the members could not be read.
    CannotRun(String),
}

impl ClearTarget {
    /// The one line `backup-run.sh` reads, and the exit code: 0, 3, 2.
    pub fn line(&self) -> (String, i32) {
        match self {
            ClearTarget::Cleared(d) => (
                format!("CLEAR TARGET: OK  ({})", crate::holders::safe(d)),
                0,
            ),
            ClearTarget::Refused(d) => (
                format!("CLEAR TARGET: REFUSED  ({})", crate::holders::safe(d)),
                3,
            ),
            ClearTarget::CannotRun(d) => (
                format!("CLEAR TARGET: CANNOT-RUN  ({})", crate::holders::safe(d)),
                2,
            ),
        }
    }
}

/// For `backup-run.sh`, the one implementation of the stop policy it shares
/// with the Rust jobs (bd DAS-Backup-Manager-su1b.12). `leftover`: the mount
/// an earlier job left busy at the target's own path — shown to be the
/// target first, then its holders cleared (the script unmounts it). Else, a
/// RAID-1 target's filesystem mounted anywhere but its path: cleared and
/// unmounted as [`ensure_targets_mounted`] does before the guarded mount.
/// `held` proves the maintenance lock.
pub fn clear_target(
    target: &Target,
    leftover: bool,
    report_only: bool,
    progress: &dyn ProgressCallback,
    _held: &MaintenanceHeld,
) -> ClearTarget {
    clear_target_with(
        &SystemRunner,
        &HOST_PROBES,
        &host_clear_site(report_only, true),
        target,
        leftover,
        progress,
    )
}

fn clear_target_with(
    runner: &dyn CommandRunner,
    probes: &MountProbes<'_>,
    site: &ClearSite<'_>,
    target: &Target,
    leftover: bool,
    progress: &dyn ProgressCallback,
) -> ClearTarget {
    if leftover {
        if !(probes.is_mountpoint)(Path::new(&target.mount)) {
            return ClearTarget::Cleared(format!("{} is not a mount point", target.mount));
        }
        if let Err(why) = mounted_is_target(runner, target) {
            return ClearTarget::Refused(format!("not the target, never touched: {why}"));
        }
        let text = match (site.mountinfo)() {
            Ok(t) => t,
            Err(e) => return ClearTarget::CannotRun(e),
        };
        let at: Vec<crate::holders::MountEntry> = crate::holders::parse_mountinfo(&text)
            .into_iter()
            .filter(|m| m.mount_point == target.mount)
            .collect();
        if let Err(busy) = maintenance_on(runner, site, target.mount_uuid.as_deref(), &target.mount)
        {
            return ClearTarget::Refused(busy);
        }
        if site.report_only {
            return match clear_logged(site, runner, &target.label, &at, progress) {
                Ok(_) => {
                    ClearTarget::Cleared(format!("dry run: nothing signalled at {}", target.mount))
                }
                Err(why) => ClearTarget::Refused(why),
            };
        }
        return match clear_logged(site, runner, &target.label, &at, progress) {
            Ok(sent) if sent.is_empty() => {
                ClearTarget::Cleared(format!("nothing holds {}", target.mount))
            }
            Ok(sent) => ClearTarget::Cleared(format!(
                "{STOPPED}: {}; nothing holds {} now",
                sent.join(", "),
                target.mount
            )),
            Err(why) => ClearTarget::Refused(why),
        };
    }
    let Some(fsid) = target.mount_uuid.as_deref().filter(|u| !u.is_empty()) else {
        return ClearTarget::CannotRun("the target has no mount_uuid".into());
    };
    let host = SystemMembers { runner, probes };
    let members = match raid_guard::MemberHost::members(&host, fsid) {
        Ok(m) => m,
        Err(e) => return ClearTarget::CannotRun(e),
    };
    match clear_mounted_elsewhere(
        runner,
        site,
        &target.label,
        &target.mount,
        fsid,
        &members,
        progress,
    ) {
        Ok(None) => ClearTarget::Cleared("not mounted anywhere else".into()),
        Ok(Some(would)) if site.report_only => {
            ClearTarget::Cleared(format!("dry run: {}", would.join("; ")))
        }
        Ok(Some(sent)) if sent.is_empty() => ClearTarget::Cleared(
            "another program's mount, held by nothing, was unmounted; the kernel has it \
             mounted nowhere"
                .into(),
        ),
        Ok(Some(sent)) => ClearTarget::Cleared(format!(
            "another program's mount was unmounted, {STOPPED}: {}; the kernel has it mounted \
             nowhere",
            sent.join(", ")
        )),
        Err(why) => ClearTarget::Refused(why),
    }
}

/// What `btrdasd recovery-os clear-drive` found (bd DAS-Backup-Manager-eg4j.19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearDrive {
    /// No partition of the drive is mounted on the host any more.
    Cleared(String),
    /// A writer, a protected process, a stacked mount or one that will not
    /// come down: the drive is not to be used.
    Refused(String),
    /// The drive, its partitions or the mount table could not be read.
    CannotRun(String),
}

impl ClearDrive {
    /// The one line `recovery-os-vm.sh` reads, and the exit code: 0, 3, 2.
    pub fn line(&self) -> (String, i32) {
        match self {
            ClearDrive::Cleared(d) => {
                (format!("CLEAR DRIVE: OK  ({})", crate::holders::safe(d)), 0)
            }
            ClearDrive::Refused(d) => (
                format!("CLEAR DRIVE: REFUSED  ({})", crate::holders::safe(d)),
                3,
            ),
            ClearDrive::CannotRun(d) => (
                format!("CLEAR DRIVE: CANNOT-RUN  ({})", crate::holders::safe(d)),
                2,
            ),
        }
    }
}

/// For `recovery-os-vm.sh` (bd DAS-Backup-Manager-eg4j.19): every mount of
/// any partition of a single-drive target's drive — found by the drive's
/// configured serial only, never by type — cleared as a job that writes the
/// whole disk clears it: the stop policy (`backup.md` §Maintenance Interlock),
/// each mount unmounted deepest first, the mount table read again. `_held`
/// proves the maintenance lock.
pub fn clear_drive(
    target: &Target,
    report_only: bool,
    progress: &dyn ProgressCallback,
    _held: &MaintenanceHeld,
) -> ClearDrive {
    clear_drive_with(
        &SystemRunner,
        &HOST_PROBES,
        &host_clear_site(report_only, true),
        target,
        progress,
    )
}

fn clear_drive_with(
    runner: &dyn CommandRunner,
    probes: &MountProbes<'_>,
    site: &ClearSite<'_>,
    target: &Target,
    progress: &dyn ProgressCallback,
) -> ClearDrive {
    let serials = target.effective_serials();
    let [serial] = serials.as_slice() else {
        return ClearDrive::CannotRun(if serials.is_empty() {
            format!("target '{}' names no drive serial", target.label)
        } else {
            format!(
                "target '{}' names {} serials: not a single-drive target",
                target.label,
                serials.len()
            )
        });
    };
    let Some(disk) = (probes.device_from_serial)(serial) else {
        return ClearDrive::CannotRun(format!("no attached drive has serial {serial}"));
    };
    let out = match runner.output(Command::new("lsblk").args(["-nrp", "-o", "NAME,TYPE", &disk])) {
        Ok(o) if o.status.success() => o,
        Ok(o) => return ClearDrive::CannotRun(failed("lsblk", &o)),
        Err(e) => return ClearDrive::CannotRun(format!("lsblk could not be run: {e}")),
    };
    // The whole disk too: a filesystem made on it directly.
    let mut names = vec![disk.clone()];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut f = line.split_whitespace();
        if let (Some(name), Some("part")) = (f.next(), f.next()) {
            names.push(name.to_string());
        }
    }
    // Each by its block device number, never by what a mount calls its
    // source; a path that is not a block device cannot be answered for.
    let mut parts = Vec::new();
    for name in names {
        match (site.block_dev)(&name) {
            Ok(dev) => parts.push(crate::holders::PartitionId { path: name, dev }),
            Err(e) => return ClearDrive::CannotRun(e),
        }
    }
    let fsid = target.mount_uuid.as_deref().filter(|u| !u.is_empty());
    match clear_partitions_with(runner, site, &target.label, &parts, fsid, progress) {
        Ok(None) => ClearDrive::Cleared(format!("nothing of {disk} is mounted on this host")),
        Ok(Some(would)) if site.report_only => {
            ClearDrive::Cleared(format!("dry run: {}", would.join("; ")))
        }
        Ok(Some(sent)) if sent.is_empty() => ClearDrive::Cleared(format!(
            "another program's mount of {disk}, held by nothing, was unmounted"
        )),
        Ok(Some(sent)) => ClearDrive::Cleared(format!(
            "another program's mount of {disk} was unmounted, {STOPPED}: {}",
            sent.join(", ")
        )),
        Err(why) => ClearDrive::Refused(why),
    }
}

/// [`take_down_leftover_with`] on this host, for a job that mounts a target
/// by its own code (the scrub engine). Its caller has already shown the
/// mount is the target.
pub fn take_down_leftover(
    mount_point: &str,
    progress: &dyn ProgressCallback,
) -> Result<(), String> {
    take_down_leftover_with(
        &SystemRunner,
        mount_point,
        progress,
        Some((&host_clear_site(false, true), mount_point, None)),
    )
    .map(drop)
}

/// Where the mount at `mount_point` came from: udisks, a systemd unit with a
/// file or a generator source, or neither (`systemctl show` through the
/// runner; one that cannot be run says nothing).
fn origin_of(runner: &dyn CommandRunner, mount_point: &str) -> crate::holders::Origin {
    let out = runner.output(Command::new("systemctl").args([
        "show",
        "--property=Id",
        "--property=LoadState",
        "--property=FragmentPath",
        "--property=SourcePath",
        "--",
        mount_point,
    ]));
    let text = match out {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        _ => None,
    };
    crate::holders::origin(mount_point, text.as_deref())
}

/// btrfs maintenance running on the filesystem (bd su1b.12 review): an
/// exclusive operation (`/sys/fs/btrfs/<fsid>/exclusive_operation` not
/// `none`: balance, device add/remove/replace, resize, swap activate) or a
/// scrub (`btrfs scrub status <mount point>` reading `running`). Either
/// refuses the clearing, as a writer does: it is never interrupted. A file
/// that cannot be read, or a status that cannot be run, refuses too.
fn maintenance_on(
    runner: &dyn CommandRunner,
    site: &ClearSite<'_>,
    fsid: Option<&str>,
    mount_point: &str,
) -> Result<(), String> {
    if let Some(fsid) = fsid {
        let f = site
            .sysfs_btrfs
            .join(fsid.to_ascii_lowercase())
            .join("exclusive_operation");
        match std::fs::read_to_string(&f) {
            Ok(op) if op.trim() == "none" || op.trim().is_empty() => {}
            Ok(op) => {
                return Err(format!(
                    "a btrfs {} is running on it ({}) — never interrupted",
                    op.trim(),
                    f.display()
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "whether btrfs maintenance runs on it cannot be read ({}: {e})",
                    f.display()
                ));
            }
        }
    }
    let out = runner.output(Command::new("btrfs").args(["scrub", "status", mount_point]));
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            if text
                .lines()
                .any(|l| l.trim_start().starts_with("Status:") && l.contains("running"))
            {
                return Err(format!(
                    "a scrub is running on it ({mount_point}) — never interrupted"
                ));
            }
            Ok(())
        }
        Ok(o) => Err(format!(
            "whether a scrub runs on it cannot be told ({})",
            failed("btrfs scrub status", &o)
        )),
        Err(e) => Err(format!(
            "whether a scrub runs on it cannot be told (btrfs scrub status: {e})"
        )),
    }
}

/// The stop policy over every process using `mounts`
/// ([`crate::holders::clear_holders`]), each holder and each signal said as a
/// warning. `Ok` the signals sent (`SIGTERM to pid 300 (dolphin; user bosco,
/// unit app-dolphin.scope)`, for the report); `Err` why it refused.
fn clear_logged(
    site: &ClearSite<'_>,
    runner: &dyn CommandRunner,
    who: &str,
    mounts: &[crate::holders::MountEntry],
    progress: &dyn ProgressCallback,
) -> Result<Vec<String>, String> {
    let sleep = |d: Duration| runner.pause(d);
    let env = crate::holders::ClearEnv {
        site: &site.proc,
        self_pid: site.self_pid,
        signaller: site.signaller,
        sleep: &sleep,
        report_only: site.report_only,
        stop_readers: site.stop_readers,
    };
    let say = |lines: &[String]| {
        for l in lines {
            progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!("Target '{who}': {l}"),
            );
        }
    };
    match crate::holders::clear_holders(&env, mounts) {
        Ok(done) => {
            say(&done.lines);
            Ok(done
                .signalled
                .iter()
                .map(|(pid, sig, cmd)| {
                    let who = done.who.get(pid).map_or("", String::as_str);
                    format!("{} to pid {pid} ({cmd}; {who})", sig.name())
                })
                .collect())
        }
        Err(r) => {
            say(&r.lines);
            Err(r.why)
        }
    }
}

/// Each of `mounts` as two aligned rows (bd DAS-Backup-Manager-3gzs.14): the
/// `mount` row (source, filesystem and rw/ro, mount point, who made it, what
/// is done about it) and the `options` row (the mount's own options, the
/// filesystem's, the directory of it shown there). `tail` is the verdict
/// after `->`.
fn mount_rows(
    runner: &dyn CommandRunner,
    mounts: &[crate::holders::MountEntry],
    tail: &str,
) -> Vec<String> {
    use crate::holders::{field, row};
    let fs_of = |m: &crate::holders::MountEntry| {
        format!(
            "{} {}",
            m.fstype,
            if m.options.split(',').any(|o| o == "ro") {
                "ro"
            } else {
                "rw"
            }
        )
    };
    let width =
        |f: &dyn Fn(&crate::holders::MountEntry) -> usize| mounts.iter().map(f).max().unwrap_or(0);
    let source_w = width(&|m| field(&m.source).chars().count());
    let fs_w = width(&|m| field(&fs_of(m)).chars().count());
    let point_w = width(&|m| field(&m.mount_point).chars().count());
    let mut rows = Vec::new();
    for m in mounts {
        rows.push(row(
            "mount",
            &format!(
                "{:<source_w$}  {:<fs_w$}  {:<point_w$}  by {}  -> {tail}",
                field(&m.source),
                field(&fs_of(m)),
                field(&m.mount_point),
                field(&origin_of(runner, &m.mount_point).to_string()),
            ),
        ));
        rows.push(row(
            "options",
            &format!(
                "mount {}  filesystem {}  root {}",
                field(&m.options),
                field(&m.super_options),
                field(&m.root)
            ),
        ));
    }
    rows
}

/// A RAID-1 target's filesystem mounted anywhere but the target's own path
/// (bd DAS-Backup-Manager-su1b.12): each such mount said (point, origin,
/// options), its holders cleared by the operator's stop policy, each mount
/// unmounted deepest first (up to [`UMOUNT_ATTEMPTS`] tries, the holders
/// looked for again between them), then the mount table read again and the
/// kernel asked whether it still has the filesystem mounted at all. Never a
/// bind of another program's mount: its devinfo cannot be trusted (a scan
/// clears a never-opened member's missing flag) and its options are not the
/// configured ones. `Ok(None)`: mounted nowhere else; `Ok(Some(signals))`:
/// cleared, with the signals sent; `Err`: why it was not, the target then NOT
/// available.
fn clear_mounted_elsewhere(
    runner: &dyn CommandRunner,
    site: &ClearSite<'_>,
    label: &str,
    mount: &str,
    fsid: &str,
    members: &[String],
    progress: &dyn ProgressCallback,
) -> Result<Option<Vec<String>>, String> {
    use crate::holders::{mounts_of, parse_mountinfo, stacked_under};
    let warn = |m: String| {
        progress.on_log(
            crate::progress::LogLevel::Warning,
            &format!("Target '{label}': {m}"),
        )
    };
    let elsewhere = |all: &[crate::holders::MountEntry]| -> Vec<crate::holders::MountEntry> {
        mounts_of(all, members)
            .into_iter()
            .filter(|m| m.mount_point != mount)
            .collect()
    };
    let all = parse_mountinfo(&(site.mountinfo)()?);
    let ours = elsewhere(&all);
    let sys = site.sysfs_btrfs.join(fsid.to_ascii_lowercase());
    let hidden = || {
        format!(
            "every mount of it this host shows is gone, yet the kernel still has it mounted \
             ({} exists): another mount namespace or a lazy unmount held open keeps it — \
             not mounted beside it",
            sys.display()
        )
    };
    if ours.is_empty() {
        // Mounted nowhere this host can see, not even at its own path, yet
        // the kernel has it mounted (bd su1b.12 review).
        if mounts_of(&all, members).is_empty() && sys.exists() {
            return Err(hidden());
        }
        return Ok(None);
    }
    for line in mount_rows(
        runner,
        &ours,
        "another program's mount, not this job's; taken down so the target is mounted as \
         configured",
    ) {
        warn(line);
    }
    let stacked = stacked_under(&all, &ours);
    if !stacked.is_empty() {
        return Err(format!(
            "another filesystem is mounted inside it ({}) — not this job's to take down",
            stacked
                .iter()
                .map(|m| format!("{} at {}", m.source, m.mount_point))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    maintenance_on(runner, site, Some(fsid), &ours[0].mount_point)?;
    if site.report_only {
        return would_clear(runner, site, label, &ours, progress).map(Some);
    }
    let mut sent = clear_logged(site, runner, label, &ours, progress)?;
    unmount_each(runner, site, label, &ours, progress, &mut sent)?;
    let left = elsewhere(&parse_mountinfo(&(site.mountinfo)()?));
    if !left.is_empty() {
        return Err(format!(
            "still mounted at {} after it was unmounted",
            left.iter()
                .map(|m| m.mount_point.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if sys.exists() {
        return Err(hidden());
    }
    if !sent.is_empty() {
        warn(crate::holders::row(
            "action",
            &format!("{STOPPED}: {}", sent.join(", ")),
        ));
    }
    Ok(Some(sent))
}

/// A dry run's account of clearing `ours`: each "would unmount", each "would
/// stop" holder; a holder the policy refuses is the `Err`, as in a real run.
fn would_clear(
    runner: &dyn CommandRunner,
    site: &ClearSite<'_>,
    who: &str,
    ours: &[crate::holders::MountEntry],
    progress: &dyn ProgressCallback,
) -> Result<Vec<String>, String> {
    let mut would: Vec<String> = ours
        .iter()
        .map(|m| {
            format!(
                "would unmount {} ({})",
                m.mount_point,
                origin_of(runner, &m.mount_point)
            )
        })
        .collect();
    clear_logged(site, runner, who, ours, progress)?;
    let env_lines = crate::holders::scan_holders(&site.proc, ours)
        .map_err(|e| format!("who uses it cannot be read ({e})"))?;
    would.extend(env_lines.iter().map(|h| format!("would stop: {h}")));
    for w in would.iter().filter(|w| w.starts_with("would unmount")) {
        progress.on_log(
            crate::progress::LogLevel::Warning,
            &format!("Target '{who}': {}", crate::holders::row("action", w)),
        );
    }
    Ok(would)
}

/// Unmount each of `ours` (already deepest first): up to [`UMOUNT_ATTEMPTS`]
/// tries, the holders looked for again between them (what that stops is
/// added to `sent`). `Err` names the mount that would not come down.
fn unmount_each(
    runner: &dyn CommandRunner,
    site: &ClearSite<'_>,
    who: &str,
    ours: &[crate::holders::MountEntry],
    progress: &dyn ProgressCallback,
    sent: &mut Vec<String>,
) -> Result<(), String> {
    for m in ours {
        let mut why = String::new();
        let mut down = false;
        for attempt in 1..=UMOUNT_ATTEMPTS {
            if attempt > 1 {
                runner.pause(UMOUNT_RETRY_PAUSE);
                sent.extend(clear_logged(
                    site,
                    runner,
                    who,
                    std::slice::from_ref(m),
                    progress,
                )?);
            }
            // Never by path alone: the mount scanned must still be the one
            // at that path (a path can be unmounted and mounted over by
            // something else between the scan and now), and the TOPMOST one
            // there — the last in the table — since `umount <path>` takes
            // the top one down (review N1).
            let now = crate::holders::parse_mountinfo(&(site.mountinfo)()?);
            match now.iter().rfind(|n| n.mount_point == m.mount_point) {
                None => {
                    down = true;
                    break;
                }
                Some(n) if n.id != m.id => {
                    return Err(format!(
                        "{} is no longer the mount that was scanned (mount id {} became {}) \
                         — not unmounted",
                        m.mount_point, m.id, n.id
                    ));
                }
                Some(_) => {}
            }
            why = match runner.status(
                Command::new("umount")
                    .arg("--no-canonicalize")
                    .arg(&m.mount_point),
            ) {
                Ok(s) if s.success() => {
                    down = true;
                    break;
                }
                Ok(s) => exit_why("umount", s),
                Err(e) => format!("umount failed to run: {e}"),
            };
        }
        if !down {
            return Err(format!(
                "{} will not unmount: {why} after {UMOUNT_ATTEMPTS} attempts",
                m.mount_point
            ));
        }
        progress.on_log(
            crate::progress::LogLevel::Warning,
            &format!(
                "Target '{who}': {}",
                crate::holders::row(
                    "action",
                    &format!("unmounted {} (another program's mount)", m.mount_point)
                )
            ),
        );
    }
    Ok(())
}

/// Every mount of `partitions` (any filesystem), by the operator's stop
/// policy (bd DAS-Backup-Manager-eg4j.19): each said (point, origin, options),
/// its holders cleared, each unmounted deepest first, then the mount table
/// read again. `btrfs_fsid`: the filesystem UUID of a btrfs on the drive —
/// maintenance on a btrfs mount refuses, and `/sys/fs/btrfs/<fsid>` must be
/// gone afterwards (or must never have been there with nothing visibly
/// mounted). `Ok(None)`: mounted nowhere; `Ok(Some(signals))`: cleared (a
/// dry run: the "would …" lines); `Err`: refused, why.
fn clear_partitions_with(
    runner: &dyn CommandRunner,
    site: &ClearSite<'_>,
    who: &str,
    partitions: &[crate::holders::PartitionId],
    btrfs_fsid: Option<&str>,
    progress: &dyn ProgressCallback,
) -> Result<Option<Vec<String>>, String> {
    use crate::holders::{mounts_of_partitions, parse_mountinfo, stacked_under};
    let warn = |m: String| {
        progress.on_log(
            crate::progress::LogLevel::Warning,
            &format!("Target '{who}': {m}"),
        )
    };
    let sys = btrfs_fsid.map(|f| site.sysfs_btrfs.join(f.to_ascii_lowercase()));
    let hidden = |sys: &Path| {
        format!(
            "every mount of it this host shows is gone, yet the kernel still has it mounted \
             ({} exists): another mount namespace or a lazy unmount held open keeps it",
            sys.display()
        )
    };
    let all = parse_mountinfo(&(site.mountinfo)()?);
    let ours = mounts_of_partitions(&all, partitions);
    if ours.is_empty() {
        if let Some(sys) = sys.as_deref().filter(|s| s.exists()) {
            return Err(hidden(sys));
        }
        return Ok(None);
    }
    for line in mount_rows(runner, &ours, "another program's mount, not this job's") {
        warn(line);
    }
    let stacked = stacked_under(&all, &ours);
    if !stacked.is_empty() {
        return Err(format!(
            "another filesystem is mounted inside it ({}) — not this job's to take down",
            stacked
                .iter()
                .map(|m| format!("{} at {}", m.source, m.mount_point))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // Every btrfs mount of the drive gets the maintenance check, the target's
    // `mount_uuid` or not (review N4): without one, the filesystem UUID is
    // the one the mount carries, and one that cannot be read refuses.
    if let Some(b) = ours.iter().find(|m| m.fstype == "btrfs") {
        let fsid = match btrfs_fsid {
            Some(f) => f.to_string(),
            None => filesystem_uuid_with(runner, &b.mount_point).ok_or_else(|| {
                format!(
                    "the filesystem UUID of the btrfs mount at {} cannot be read, so whether \
                     btrfs maintenance runs on it cannot be told",
                    b.mount_point
                )
            })?,
        };
        maintenance_on(runner, site, Some(&fsid), &b.mount_point)?;
    }
    if site.report_only {
        return would_clear(runner, site, who, &ours, progress).map(Some);
    }
    let mut sent = clear_logged(site, runner, who, &ours, progress)?;
    unmount_each(runner, site, who, &ours, progress, &mut sent)?;
    let left = mounts_of_partitions(&parse_mountinfo(&(site.mountinfo)()?), partitions);
    if !left.is_empty() {
        return Err(format!(
            "still mounted at {} after it was unmounted",
            left.iter()
                .map(|m| m.mount_point.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(sys) = sys.as_deref().filter(|s| s.exists()) {
        return Err(hidden(sys));
    }
    if !sent.is_empty() {
        warn(crate::holders::row(
            "action",
            &format!("{STOPPED}: {}", sent.join(", ")),
        ));
    }
    Ok(Some(sent))
}

/// Another program's mount of the filesystem `fsid` anywhere but `mount`,
/// cleared by the stop policy for a job (bd DAS-Backup-Manager-eg4j.19): the
/// members are the partitions `blkid` lists for the UUID, so a single drive
/// is served as a RAID-1 one is. Under a handed-down maintenance lock
/// (`delegated`) nothing is cleared, and any such mount is the `Err`: the
/// parent owns the mounts, and another program's is never used as found.
/// `Ok(None)`: mounted nowhere else; `Ok(Some(signals))`: cleared.
#[allow(clippy::too_many_arguments)] // one host, one filesystem, one job
fn clear_elsewhere_with(
    runner: &dyn CommandRunner,
    probes: &MountProbes<'_>,
    site: &ClearSite<'_>,
    label: &str,
    fsid: &str,
    mount: &str,
    delegated: bool,
    progress: &dyn ProgressCallback,
) -> Result<Option<Vec<String>>, String> {
    let host = SystemMembers { runner, probes };
    let members = raid_guard::MemberHost::members(&host, fsid)?;
    if delegated {
        let all = crate::holders::parse_mountinfo(&(site.mountinfo)()?);
        let elsewhere: Vec<String> = crate::holders::mounts_of(&all, &members)
            .into_iter()
            .filter(|m| m.mount_point != mount)
            .map(|m| m.mount_point)
            .collect();
        return if elsewhere.is_empty() {
            Ok(None)
        } else {
            Err(format!(
                "its filesystem is mounted by another program at {} — never bound, and not \
                 cleared under a handed-down maintenance lock",
                elsewhere.join(", ")
            ))
        };
    }
    clear_mounted_elsewhere(runner, site, label, mount, fsid, &members, progress)
}

/// [`clear_elsewhere_with`] on this host for the scrub engine's mount of a
/// single-drive target (bd DAS-Backup-Manager-eg4j.19): a scrub repairs, so
/// it stops readers as a writing job does. `held` says whether the lock was
/// handed down.
pub fn clear_filesystem_elsewhere(
    label: &str,
    fsid: &str,
    mount: &str,
    held: &MaintenanceHeld,
    progress: &dyn ProgressCallback,
) -> Result<Option<Vec<String>>, String> {
    clear_elsewhere_with(
        &SystemRunner,
        &HOST_PROBES,
        &host_clear_site(false, true),
        label,
        fsid,
        mount,
        held.is_delegated(),
        progress,
    )
}

/// Whether what is mounted at `target.mount` is the target: its filesystem
/// UUID when one is configured, else its drive's serial, as
/// [`verify_write_targets`] has it. `Err` says why it is not, or why it
/// cannot be told.
fn mounted_is_target(
    runner: &dyn CommandRunner,
    target: &crate::config::Target,
) -> Result<(), String> {
    match target.mount_uuid.as_deref().filter(|u| !u.is_empty()) {
        Some(expected) => match filesystem_uuid_with(runner, &target.mount) {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => Err(format!(
                "'{}' has filesystem UUID '{actual}', expected '{expected}'",
                target.mount
            )),
            None => Err(format!(
                "could not determine the filesystem UUID mounted at '{}' (expected \
                 '{expected}')",
                target.mount
            )),
        },
        None => verify_legacy_serial(runner, target).map(|_| ()),
    }
}

/// What the end-of-job check needs about one guarded mount.
struct GuardedMount {
    label: String,
    fsid: String,
    mount: String,
    serials: Vec<String>,
    mount_opts: String,
    record_dir: std::path::PathBuf,
    /// The kernel's view, read at the end (production: `/sys/fs/btrfs`).
    missing: MissingProbe,
}

/// Where a guarded mount keeps its record, and how its end-of-job check
/// reads the kernel's view.
struct RaidSite<'a> {
    record_dir: &'a Path,
    missing: MissingProbe,
    /// What clearing another program's mount of a target reads and does
    /// (bd DAS-Backup-Manager-su1b.12).
    clear: &'a ClearSite<'a>,
}

/// How the end-of-job check reads the kernel's missing devids.
type MissingProbe = std::rc::Rc<dyn Fn(&str) -> Result<Vec<u64>, String>>;

impl MountGuard {
    fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            newly_mounted: Vec::new(),
            guarded: Vec::new(),
            runner,
            held_kind: "targets",
            esps: Vec::new(),
            clear_notes: Vec::new(),
        }
    }

    /// What clearing did to other programs' processes, per target.
    pub fn clear_notes(&self) -> &[ClearNote] {
        &self.clear_notes
    }

    /// Note `target`'s ESP for release, when it is a mirror with an
    /// `esp_partuuid` (the only ESP a reading mounts).
    fn note_esp(&mut self, target: &crate::config::Target, root: &Path) {
        if target.role != crate::config::TargetRole::Mirror {
            return;
        }
        let Some(pu) = target.esp_partuuid.as_deref() else {
            return;
        };
        let dir = root.join(esp_dir_name(&target.label));
        self.esps.push((
            dir.to_string_lossy().into_owned(),
            format!("PARTUUID={}", pu.to_ascii_lowercase()),
        ));
    }

    /// The end-of-job check of the guarded mount at `mount_point`, if this
    /// guard made one (bd DAS-Backup-Manager-su1b.14): taken out of the list,
    /// run, and said — a recorded drop or a problem as a warning.
    fn end_check(&mut self, mount_point: &str, say: &mut dyn FnMut(bool, String)) {
        let Some(at) = self.guarded.iter().position(|g| g.mount == mount_point) else {
            return;
        };
        let g = self.guarded.remove(at);
        let probes = MountProbes {
            missing_devids: &*g.missing,
            ..HOST_PROBES
        };
        let host = SystemMembers {
            runner: &*self.runner,
            probes: &probes,
        };
        let plan = raid_guard::Plan {
            label: &g.label,
            fsid: &g.fsid,
            mount: &g.mount,
            serials: &g.serials,
            mount_opts: &g.mount_opts,
            wait: Duration::ZERO,
            record_dir: &g.record_dir,
            intent: raid_guard::Job::Backup.intent(),
        };
        let check = raid_guard::check_before_unmount(&host, &plan);
        let warn = !matches!(check, raid_guard::EndCheck::Whole);
        say(
            warn,
            format!(
                "Target '{}': {}",
                g.label,
                raid_guard::end_check_line(&check)
            ),
        );
    }

    /// A guard over the source volumes rather than the targets.
    fn for_sources(runner: Arc<dyn CommandRunner>) -> Self {
        let mut guard = Self::new(runner);
        guard.held_kind = "sources";
        guard
    }

    /// `umount` one mount point, up to [`UMOUNT_ATTEMPTS`] times with
    /// [`UMOUNT_RETRY_PAUSE`] between attempts. `Err` carries why the last
    /// attempt failed; `retrying` is told about each earlier one.
    fn release(&self, mount_point: &str, retrying: impl FnMut(&str)) -> Result<(), String> {
        umount_retrying(&*self.runner, mount_point, retrying)
    }

    /// After a `mount` at `mount_point` that did not succeed: a mount helper
    /// can fail after the kernel mounted, and that mount is this job's — the
    /// path was not a mount point before it (bd DAS-Backup-Manager-1fpw;
    /// `backup-run.sh` records ownership before mounting). It is taken down
    /// now; one that will not come down stays this guard's, so the job's own
    /// unmount tries again and fails the job if it stays.
    fn take_back_failed_mount(
        &mut self,
        is_mountpoint: &dyn Fn(&Path) -> bool,
        mount_point: &str,
        what: &str,
        progress: &dyn ProgressCallback,
    ) {
        use crate::progress::LogLevel;
        if !is_mountpoint(Path::new(mount_point)) {
            return;
        }
        progress.on_log(
            LogLevel::Warning,
            &format!(
                "{what}: mount reported a failure, yet {mount_point} is mounted — unmounting it"
            ),
        );
        if let Err(why) = self.release(mount_point, |msg| progress.on_log(LogLevel::Warning, msg)) {
            progress.on_log(
                LogLevel::Error,
                &format!(
                    "{what}: {mount_point} is still mounted: {why} after {UMOUNT_ATTEMPTS} attempts"
                ),
            );
            self.newly_mounted.push(mount_point.to_string());
        }
    }

    /// Unmount everything this guard mounted, last-mounted first, retrying
    /// each, with progress reporting. Returns the mount points that are STILL
    /// MOUNTED after every attempt — the caller must fail its job on a
    /// non-empty list ([`still_mounted_error`]): a backup target left mounted
    /// is exposed, and the job is not a success. Prefer this over relying on
    /// `Drop`, which can only print.
    #[must_use = "a mount point left mounted must fail the job (see still_mounted_error)"]
    pub fn unmount(&mut self, progress: &dyn ProgressCallback) -> Vec<String> {
        let mut still_mounted = self.release_esps(progress);
        if self.newly_mounted.is_empty() {
            return still_mounted;
        }
        let total = self.newly_mounted.len() as u64;
        progress.on_stage(&format!("Unmounting {}", self.held_kind), total);
        let held: Vec<String> = self.newly_mounted.drain(..).rev().collect();
        for (i, mount_point) in held.into_iter().enumerate() {
            self.end_check(&mount_point, &mut |warn, line| {
                progress.on_log(
                    if warn {
                        crate::progress::LogLevel::Warning
                    } else {
                        crate::progress::LogLevel::Info
                    },
                    &line,
                )
            });
            let released = self.release(&mount_point, |msg| {
                progress.on_log(crate::progress::LogLevel::Warning, msg);
            });
            match released {
                Ok(()) => {
                    progress.on_progress((i + 1) as u64, total, &format!("Unmounted {mount_point}"))
                }
                Err(why) => {
                    progress.on_log(
                        crate::progress::LogLevel::Error,
                        &format!(
                            "{mount_point} is still mounted: {why} after {UMOUNT_ATTEMPTS} attempts"
                        ),
                    );
                    still_mounted.push(mount_point);
                }
            }
        }
        still_mounted
    }

    /// Take down each noted ESP that `findmnt` shows mounted at its own
    /// directory; anything else mounted from it, or nothing, is left as it
    /// is. Returns the directories still mounted, or that could not be told
    /// about: either fails the job, as a target left mounted does.
    fn release_esps(&mut self, progress: &dyn ProgressCallback) -> Vec<String> {
        use crate::progress::LogLevel;
        let mut still = Vec::new();
        for (dir, spec) in std::mem::take(&mut self.esps) {
            let out = self
                .runner
                .output(Command::new("findmnt").args(["-n", "-o", "TARGET", "-S", &spec]));
            let here = match out {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).lines().any(|l| l == dir)
                }
                // 1: mounted nowhere.
                Ok(o) if o.status.code() == Some(1) => false,
                Ok(o) => {
                    progress.on_log(
                        LogLevel::Error,
                        &format!(
                            "Could not tell whether {dir} holds {spec}: {}",
                            exit_why("findmnt", o.status)
                        ),
                    );
                    still.push(dir);
                    continue;
                }
                Err(e) => {
                    progress.on_log(
                        LogLevel::Error,
                        &format!(
                            "Could not tell whether {dir} holds {spec}: findmnt failed to run: {e}"
                        ),
                    );
                    still.push(dir);
                    continue;
                }
            };
            if !here {
                continue;
            }
            progress.on_log(
                LogLevel::Warning,
                &format!("{dir}: a recovery drive's ESP left mounted by a recovery-OS reading — unmounting it"),
            );
            if let Err(why) = take_down_leftover_with(&*self.runner, &dir, progress, None) {
                progress.on_log(LogLevel::Error, &format!("{dir} is still mounted: {why}"));
                still.push(dir);
            }
        }
        still
    }

    /// How many mount points this guard is responsible for.
    pub fn count(&self) -> usize {
        self.newly_mounted.len()
    }

    /// Unmount everything still held, in reverse order and with the same
    /// retries as [`MountGuard::unmount`], and return one message per mount
    /// point left mounted. `Drop` has nowhere to send these but stderr; they
    /// are returned rather than printed here so that "a failed unmount is
    /// reported, a clean one is not" can be tested.
    fn unmount_remaining(&mut self) -> Vec<String> {
        let held: Vec<String> = self.newly_mounted.drain(..).rev().collect();
        held.into_iter()
            .filter_map(|mount_point| {
                self.end_check(&mount_point, &mut |warn, line| {
                    if warn {
                        eprintln!("MountGuard::drop: {line}");
                    }
                });
                self.release(&mount_point, |msg| eprintln!("MountGuard::drop: {msg}"))
                    .err()
                    .map(|why| {
                        format!(
                            "MountGuard::drop: {mount_point} is still mounted: {why} after \
                             {UMOUNT_ATTEMPTS} attempts"
                        )
                    })
            })
            .collect()
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        // Safety net: unmount anything not yet explicitly unmounted. This is the
        // SOLE cleanup path when a job panics or is aborted mid-flight, so a
        // swallowed failure here leaves a target mounted and invisible — and the
        // next run's mountpoint reasoning is then wrong. Drop cannot return a
        // Result and has no ProgressCallback, so the failure goes to stderr,
        // which journald captures for the helper daemon
        // (bd DAS-Backup-Manager-06p).
        for failure in self.unmount_remaining() {
            eprintln!("{failure}");
        }
    }
}

/// Take back a mount-point directory this call created, after the mount it
/// was created for failed.
///
/// A bare directory left at a mount path is the artifact the bare-mountpoint
/// guard has to clean up or abort on: whatever writes there next lands on the
/// filesystem underneath (bd DAS-Backup-Manager-9on). `create_mount_points` in
/// `scripts/backup-run.sh` rmdirs it for an unavailable target; this is the
/// same rule for the Rust path. `remove_dir` only removes an EMPTY directory,
/// so anything the failed mount left inside stays put and is reported.
/// `owner` names the target or source in the warning.
fn remove_created_mount_point(owner: &str, mount_point: &str, progress: &dyn ProgressCallback) {
    if let Err(e) = std::fs::remove_dir(mount_point) {
        progress.on_log(
            crate::progress::LogLevel::Warning,
            &format!(
                "{owner}: could not remove mount point '{mount_point}' created for the \
                 failed mount ({e}) — a bare directory is left there"
            ),
        );
    }
}

/// Mount all configured backup targets that are not already mounted.
///
/// For each target in `config.targets`:
/// 1. Skip if already mounted (checked via `/proc/mounts`).
/// 2. With `mount_uuid` set: available when any configured serial is present
///    or a filesystem with that UUID is (`blkid -U`), and mounted as
///    `UUID=<uuid>`, falling back to the serial's partition. Without it:
///    the first present serial's partition for the target's role.
/// 3. `mkdir -p` the mount point, then `mount -t btrfs -o <opts>`.
/// 4. Track newly-mounted targets in the returned [`MountGuard`].
///
/// Returns `Err(MountError::NoDrivesFound)` only if **no** targets could be
/// mounted *and* no targets were already mounted. Individual mount failures
/// are logged as warnings but do not abort the operation.
///
/// `held` is the proof that this process holds the DAS maintenance lock, and
/// says whether the hold was handed down by a parent job that mounted the
/// targets for this run (`is_delegated`). Requiring it means no job can mount
/// a target another job is using — a backup, a scrub, or a recovery drive a
/// VM has mounted, which a second kernel's mount would corrupt
/// (bd DAS-Backup-Manager-frb). Release the guard before the lock.
pub fn ensure_targets_mounted(
    config: &Config,
    job: raid_guard::Job,
    progress: &dyn ProgressCallback,
    held: &MaintenanceHeld,
) -> Result<MountGuard, MountError> {
    ensure_targets_mounted_on(config, job, progress, held, false)
}

/// [`ensure_targets_mounted`] for a dry run (bd DAS-Backup-Manager-su1b.12
/// review): another program's mount of a RAID-1 target, and the holders of a
/// busy leftover, are only said — nothing signalled, nothing of another
/// program's unmounted; such a target is NOT available for the dry run.
pub fn ensure_targets_mounted_dry_run(
    config: &Config,
    job: raid_guard::Job,
    progress: &dyn ProgressCallback,
    held: &MaintenanceHeld,
) -> Result<MountGuard, MountError> {
    ensure_targets_mounted_on(config, job, progress, held, true)
}

fn ensure_targets_mounted_on(
    config: &Config,
    job: raid_guard::Job,
    progress: &dyn ProgressCallback,
    held: &MaintenanceHeld,
    report_only: bool,
) -> Result<MountGuard, MountError> {
    ensure_targets_mounted_with(
        config,
        job.intent(),
        progress,
        held,
        &HOST_PROBES,
        Arc::new(SystemRunner),
        RaidSite {
            record_dir: &raid_guard::record_dir(),
            missing: std::rc::Rc::new(raid_guard::missing_devids),
            clear: &host_clear_site(report_only, job.stops_readers()),
        },
    )
}

/// Where a target's drive is, found the way [`ensure_targets_mounted`] mounts
/// it: by the configured serials, and by `mount_uuid` when one is set. Only
/// identities the config names are looked up; nothing is enumerated.
struct Located<'a> {
    serials: Vec<String>,
    serial_dev: Option<String>,
    uuid: Option<&'a str>,
    /// `mount_uuid`, when a configured serial is present or a filesystem with
    /// that UUID is on this host.
    by_uuid: Option<&'a str>,
    /// The partition a present serial's role names, when it exists.
    part_dev: Option<String>,
}

fn locate_target<'a>(target: &'a Target, probes: &MountProbes<'_>) -> Located<'a> {
    let serials = target.effective_serials();
    let serial_dev = serials.iter().find_map(|s| (probes.device_from_serial)(s));
    let uuid = target.mount_uuid.as_deref().filter(|u| !u.is_empty());
    let by_uuid = uuid.filter(|u| serial_dev.is_some() || (probes.device_for_uuid)(u).is_some());
    let part_dev = serial_dev
        .as_deref()
        .map(|dev| partition_device(dev, &target.role))
        .filter(|p| Path::new(p).exists());
    Located {
        serials,
        serial_dev,
        uuid,
        by_uuid,
        part_dev,
    }
}

/// Whether `target`'s drive is attached to this host: what
/// [`ensure_targets_mounted`] needs to mount it at all. `false` is the state
/// it skips with a warning; a check that treats a target as absent must use
/// this, never a mount that failed, so "not attached" and "would not mount"
/// stay apart.
pub fn target_attached(target: &Target) -> bool {
    target_attached_with(target, &HOST_PROBES)
}

fn target_attached_with(target: &Target, probes: &MountProbes<'_>) -> bool {
    let found = locate_target(target, probes);
    found.by_uuid.is_some() || found.part_dev.is_some()
}

/// [`ensure_targets_mounted`] against an explicit host: `probes` answers what
/// is mounted and where the drives are, `runner` executes `mount`.
fn ensure_targets_mounted_with(
    config: &Config,
    intent: raid_guard::Intent,
    progress: &dyn ProgressCallback,
    held: &MaintenanceHeld,
    probes: &MountProbes<'_>,
    runner: Arc<dyn CommandRunner>,
    raid: RaidSite<'_>,
) -> Result<MountGuard, MountError> {
    let RaidSite {
        record_dir: raid_dir,
        missing: end_missing,
        clear,
    } = raid;
    let mut guard = MountGuard::new(runner);
    let mut any_available = false;
    // The targets skipped because their clearing refused (bd su1b.31).
    let mut refused: Vec<ClearNote> = Vec::new();
    let total = config.targets.len() as u64;

    if total == 0 {
        return Ok(guard);
    }

    progress.on_stage("Mounting targets", total);

    for (i, target) in config.targets.iter().enumerate() {
        let mount_path = Path::new(&target.mount);
        // Under a handed-down lock the parent releases its own (`unmount_all`).
        if !held.is_delegated() {
            guard.note_esp(target, Path::new(ESP_MOUNT_ROOT));
        }

        // Already mounted at the configured path.
        if mount_path.exists() && (probes.is_mountpoint)(mount_path) {
            // Under a handed-down lock the parent (`backup-run.sh`) mounted it
            // for this run and takes it down itself: used as found.
            if held.is_delegated() {
                any_available = true;
                progress.on_progress(
                    (i + 1) as u64,
                    total,
                    &format!("{} already mounted", target.label),
                );
                progress.on_log(
                    crate::progress::LogLevel::Info,
                    &format!(
                        "Target '{}': {} is mounted by the job that handed down the \
                         maintenance lock; left mounted for it",
                        target.label, target.mount
                    ),
                );
                continue;
            }
            // An owned hold: no job of this project is using the target, so
            // this is a mount an earlier job failed to give back
            // (bd DAS-Backup-Manager-jgm). Shown to be the target, it is
            // taken down and mounted afresh below, so this run owns — and
            // gives back — the mount it uses, with its own options. What
            // cannot be shown to be the target, or cannot be taken down,
            // refuses the job before it mounts anything.
            let foreign = |detail| MountError::ForeignMount {
                label: target.label.clone(),
                mount: target.mount.clone(),
                detail,
            };
            mounted_is_target(&*guard.runner, target).map_err(foreign)?;
            let stopped = take_down_leftover_with(
                &*guard.runner,
                &target.mount,
                progress,
                Some((clear, &target.label, target.mount_uuid.as_deref())),
            )
            .map_err(|detail| MountError::LeftoverMounted {
                label: target.label.clone(),
                mount: target.mount.clone(),
                detail,
            })?;
            if !stopped.is_empty() {
                guard.clear_notes.push(ClearNote {
                    label: target.label.clone(),
                    outcome: ClearOutcome::Stopped,
                    detail: format!("{STOPPED}: {}", stopped.join(", ")),
                });
            }
            progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!(
                    "Target '{}': {} was left mounted by an earlier job — unmounted, to be \
                     mounted afresh for this run",
                    target.label, target.mount
                ),
            );
        }

        // A RAID-1 target another program mounted elsewhere (bd
        // DAS-Backup-Manager-su1b.12): never bound — that mount is cleared
        // by the operator's stop policy and the target mounted through the
        // guard below; what cannot be cleared leaves it NOT available. Under
        // a handed-down lock nothing changes: the parent owns the mounts.
        //
        // Under a handed-down hold such a mount is not cleared either, and
        // still never bound: the target is NOT available (the parent may
        // have skipped it for a writer that holds it).
        // Every target with a `mount_uuid` — a RAID-1 one or a single drive
        // (a recovery drive) — is looked for by its filesystem UUID, and its
        // other mounts are cleared, never used as found or written through
        // (bd DAS-Backup-Manager-eg4j.19). Only a target with no `mount_uuid`
        // has nothing to look for, and keeps the bind fallback below.
        let clearable = target.mount_uuid.as_deref().is_some_and(|u| !u.is_empty());
        if clearable {
            let fsid = target.mount_uuid.as_deref().unwrap_or_default();
            let cleared = clear_elsewhere_with(
                &*guard.runner,
                probes,
                clear,
                &target.label,
                fsid,
                &target.mount,
                held.is_delegated(),
                progress,
            )
            .and_then(|done| match done {
                // A dry run touched nothing: that mount is still there, and
                // the target is never mounted beside it.
                Some(would) if clear.report_only => Err(format!(
                    "dry run: {} — nothing was stopped or unmounted",
                    would.join("; ")
                )),
                other => Ok(other),
            });
            // What it did to other programs' processes, for the report (bd
            // su1b.31): stopped, would stop (a dry run), or skipped.
            match &cleared {
                Ok(Some(sent)) if !sent.is_empty() => guard.clear_notes.push(ClearNote {
                    label: target.label.clone(),
                    outcome: ClearOutcome::Stopped,
                    detail: format!("{STOPPED}: {}", sent.join(", ")),
                }),
                Err(why) => {
                    let outcome = if clear.report_only {
                        ClearOutcome::WouldStop
                    } else {
                        ClearOutcome::Refused
                    };
                    let note = ClearNote {
                        label: target.label.clone(),
                        outcome,
                        detail: why.clone(),
                    };
                    refused.push(note.clone());
                    guard.clear_notes.push(note);
                }
                _ => {}
            }
            if let Err(why) = cleared {
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!(
                        "Target '{}': {why} — skipping, this target is NOT available",
                        target.label
                    ),
                );
                continue;
            }
        }

        // Already mounted elsewhere (e.g. udisks2 at /run/media/) — bind-mount
        // at the configured path so btrbk can find the target where it expects it.
        if let Some(actual) =
            (probes.find_mount_for_device)(&target.serial, &target.role).filter(|_| !clearable)
        {
            // Create configured mount point directory if needed
            let created = !mount_path.exists();
            if created && let Err(e) = std::fs::create_dir_all(&target.mount) {
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!(
                        "Target '{}': could not create mount point {} for bind mount ({e}) \
                         — skipping, this target is NOT available",
                        target.label, target.mount
                    ),
                );
                continue;
            }
            let status = guard.runner.status(
                Command::new("mount")
                    .arg("--bind")
                    .arg(&actual)
                    .arg(&target.mount),
            );
            match status {
                Ok(s) if s.success() => {
                    guard.newly_mounted.push(target.mount.clone());
                    any_available = true;
                    progress.on_progress(
                        (i + 1) as u64,
                        total,
                        &format!(
                            "{} bind-mounted {} → {}",
                            target.label, actual, target.mount
                        ),
                    );
                    // A bind mount takes that mount as another program made
                    // it: the configured options cannot be applied, so say
                    // which are not in force (bd DAS-Backup-Manager-804).
                    if !config.das.mount_opts.is_empty() {
                        progress.on_log(
                            crate::progress::LogLevel::Warning,
                            &format!(
                                "Target '{}': mounted at {} by another program, not by this \
                                 job: [das].mount_opts ({}) were not applied — that mount's \
                                 own options are in force",
                                target.label, actual, config.das.mount_opts
                            ),
                        );
                    }
                }
                _ => {
                    progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!(
                            "{}: bind mount {} → {} failed — skipping, this target is \
                             NOT available",
                            target.label, actual, target.mount
                        ),
                    );
                    guard.take_back_failed_mount(
                        probes.is_mountpoint,
                        &target.mount,
                        &format!("Target '{}'", target.label),
                        progress,
                    );
                    if created {
                        remove_created_mount_point(
                            &format!("Target '{}'", target.label),
                            &target.mount,
                            progress,
                        );
                    }
                }
            }
            continue;
        }

        // Find the target the way `backup-run.sh` does (check_das_connected,
        // mount_targets; bd DAS-Backup-Manager-arx). With `mount_uuid` set it
        // is AVAILABLE when any configured serial is present OR a filesystem
        // with that UUID is on this host, and it is mounted BY UUID — BTRFS
        // assembles a RAID-1 from whichever member is present. Without it,
        // the legacy route: the first configured serial present, and the
        // partition its role names. Only identities the config names are
        // looked up; nothing is enumerated.
        let Located {
            serials,
            serial_dev,
            uuid,
            by_uuid,
            part_dev,
        } = locate_target(target, probes);

        if by_uuid.is_none() && part_dev.is_none() {
            let why = match (uuid, &serial_dev) {
                (Some(u), _) => format!(
                    "no configured drive (serials {}) is present and no filesystem has \
                     UUID {u}",
                    if serials.is_empty() {
                        "none".to_string()
                    } else {
                        serials.join(", ")
                    }
                ),
                (None, Some(dev)) => format!(
                    "partition '{}' not found",
                    partition_device(dev, &target.role)
                ),
                (None, None) => format!("drive serial '{}' not found", serials.join(", ")),
            };
            progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!("Target '{}': {why} — skipping", target.label),
            );
            continue;
        }

        // Ensure mount point directory exists
        let created = !mount_path.exists();
        if created && let Err(e) = std::fs::create_dir_all(&target.mount) {
            progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!(
                    "Target '{}': could not create '{}' ({e}) — skipping",
                    target.label, target.mount
                ),
            );
            continue;
        }

        // A RAID-1 target: the guarded mount, never `degraded` as configured
        // and never the partition fallback (bd DAS-Backup-Manager-su1b.10).
        if raid_guard::is_guarded(target) {
            match mount_guarded_with(
                config,
                target,
                intent,
                raid_dir,
                progress,
                probes,
                &*guard.runner,
            ) {
                Ok(outcome) => {
                    any_available = true;
                    guard.newly_mounted.push(target.mount.clone());
                    // A normal read-write mount is checked for a dropped
                    // member at its end (bd DAS-Backup-Manager-su1b.14). A
                    // degraded one shows its absent member missing all its
                    // life, its period recorded at the mount; a read-only one
                    // writes nothing: neither has anything to record.
                    if matches!(outcome, raid_guard::Outcome::Normal { .. })
                        && !raid_guard::is_read_only(&config.das.mount_opts)
                    {
                        guard.guarded.push(GuardedMount {
                            label: target.label.clone(),
                            fsid: target.mount_uuid.clone().unwrap_or_default(),
                            mount: target.mount.clone(),
                            serials: target.effective_serials(),
                            mount_opts: config.das.mount_opts.clone(),
                            record_dir: raid_dir.to_path_buf(),
                            missing: end_missing.clone(),
                        });
                    }
                    if let raid_guard::Outcome::Normal {
                        rejoined_now: true, ..
                    } = outcome
                    {
                        raid_guard::note_owed_scrub(&target.label);
                    }
                    let (status, _) = outcome.status_line();
                    progress.on_progress(
                        (i + 1) as u64,
                        total,
                        &format!(
                            "Mounted {} at {} (UUID={}){}",
                            target.label,
                            target.mount,
                            target.mount_uuid.as_deref().unwrap_or_default(),
                            if status == "OK" {
                                String::new()
                            } else {
                                format!(" — {status}")
                            }
                        ),
                    );
                }
                Err(raid_guard::Failure::Refused { why, left_mounted }) => {
                    if left_mounted {
                        guard.newly_mounted.push(target.mount.clone());
                    } else if created {
                        remove_created_mount_point(
                            &format!("Target '{}'", target.label),
                            &target.mount,
                            progress,
                        );
                    }
                    return Err(MountError::RaidRefused {
                        label: target.label.clone(),
                        detail: why,
                    });
                }
                Err(failure) => {
                    progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!(
                            "Target '{}': {} — skipping, this target is NOT available",
                            target.label,
                            failure.why()
                        ),
                    );
                    guard.take_back_failed_mount(
                        probes.is_mountpoint,
                        &target.mount,
                        &format!("Target '{}'", target.label),
                        progress,
                    );
                    if created {
                        remove_created_mount_point(
                            &format!("Target '{}'", target.label),
                            &target.mount,
                            progress,
                        );
                    }
                }
            }
            continue;
        }

        // By UUID first when configured; the partition of a present serial
        // is the fallback, as in backup-run.sh.
        let mut devices: Vec<String> = Vec::new();
        if let Some(u) = by_uuid {
            devices.push(format!("UUID={u}"));
        }
        devices.extend(part_dev);
        let mut mounted = false;
        for (n, dev) in devices.iter().enumerate() {
            if n > 0 {
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!(
                        "Target '{}': falling back to the device-based mount ({dev})",
                        target.label
                    ),
                );
            }
            let mut cmd = Command::new("mount");
            cmd.args(["-t", "btrfs"]);
            if !config.das.mount_opts.is_empty() {
                cmd.args(["-o", &config.das.mount_opts]);
            }
            cmd.arg(dev).arg(&target.mount);
            match guard.runner.output(&mut cmd) {
                Ok(output) if output.status.success() => {
                    any_available = true;
                    guard.newly_mounted.push(target.mount.clone());
                    progress.on_progress(
                        (i + 1) as u64,
                        total,
                        &if dev.starts_with("UUID=") {
                            format!("Mounted {} at {} ({dev})", target.label, target.mount)
                        } else {
                            format!("Mounted {} at {}", target.label, target.mount)
                        },
                    );
                    mounted = true;
                    break;
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!(
                            "Target '{}': mount {} → {} failed: {}",
                            target.label,
                            dev,
                            target.mount,
                            stderr.trim()
                        ),
                    );
                    guard.take_back_failed_mount(
                        probes.is_mountpoint,
                        &target.mount,
                        &format!("Target '{}'", target.label),
                        progress,
                    );
                }
                Err(e) => {
                    progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!("Target '{}': failed to execute mount: {e}", target.label),
                    );
                }
            }
        }
        if created && !mounted {
            remove_created_mount_point(
                &format!("Target '{}'", target.label),
                &target.mount,
                progress,
            );
        }
    }

    // If no targets are available at all, that's an error
    if !any_available {
        // Drop the guard (it will try to unmount anything we managed to mount,
        // but if !any_available the guard is empty)
        if !refused.is_empty() {
            return Err(MountError::AllTargetsRefused { notes: refused });
        }
        return Err(MountError::NoDrivesFound);
    }

    Ok(guard)
}

/// Mount source top-level BTRFS volumes (`subvolid=5`) so btrbk can access
/// subvolumes for snapshotting.
///
/// Each source in `config.sources` specifies a `volume` (mount point like
/// `/.btrfs-nvme`) and a `device` (block device like `/dev/nvme1n1p2`).
/// If the volume isn't already mounted, we mount it with `subvolid=5`.
///
/// Returns a [`MountGuard`] that unmounts only newly-mounted volumes on drop.
pub fn ensure_sources_mounted(config: &Config, progress: &dyn ProgressCallback) -> MountGuard {
    ensure_sources_mounted_with(config, progress, &HOST_PROBES, Arc::new(SystemRunner))
}

/// [`ensure_sources_mounted`] against an explicit host.
fn ensure_sources_mounted_with(
    config: &Config,
    progress: &dyn ProgressCallback,
    probes: &MountProbes<'_>,
    runner: Arc<dyn CommandRunner>,
) -> MountGuard {
    let mut guard = MountGuard::for_sources(runner);

    // Deduplicate: multiple sources can share a volume (e.g. hdd-projects
    // and hdd-audiobooks both use /.btrfs-hdd).
    let mut seen_volumes = std::collections::HashSet::new();

    for source in &config.sources {
        if !seen_volumes.insert(source.volume.clone()) {
            continue;
        }

        let mount_path = Path::new(&source.volume);

        // Already mounted — nothing to do.
        if mount_path.exists() && (probes.is_mountpoint)(mount_path) {
            progress.on_log(
                crate::progress::LogLevel::Info,
                &format!("Source volume {} already mounted", source.volume),
            );
            continue;
        }

        // Create mount point if needed.
        let created = !mount_path.exists();
        if created && let Err(e) = std::fs::create_dir_all(&source.volume) {
            progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!(
                    "Source '{}': could not create '{}' ({e}) — skipping",
                    source.label, source.volume
                ),
            );
            continue;
        }

        // Mount with subvolid=5 to expose the top-level BTRFS tree.
        let mount_result = guard.runner.output(Command::new("mount").args([
            "-o",
            "subvolid=5",
            &source.device,
            &source.volume,
        ]));

        let mounted = match mount_result {
            Ok(output) if output.status.success() => {
                guard.newly_mounted.push(source.volume.clone());
                progress.on_log(
                    crate::progress::LogLevel::Info,
                    &format!(
                        "Source '{}': mounted {} at {} (subvolid=5)",
                        source.label, source.device, source.volume
                    ),
                );
                true
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!(
                        "Source '{}': mount {} → {} failed: {}",
                        source.label,
                        source.device,
                        source.volume,
                        stderr.trim()
                    ),
                );
                guard.take_back_failed_mount(
                    probes.is_mountpoint,
                    &source.volume,
                    &format!("Source '{}'", source.label),
                    progress,
                );
                false
            }
            Err(e) => {
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!("Source '{}': failed to execute mount: {e}", source.label),
                );
                false
            }
        };
        if created && !mounted {
            remove_created_mount_point(
                &format!("Source '{}'", source.label),
                &source.volume,
                progress,
            );
        }
    }

    // Create snapshot directories inside now-mounted source volumes (btrbk
    // requires them to exist before it snapshots).
    for source in &config.sources {
        let volume = Path::new(&source.volume);
        let snap_dir = volume.join(&source.snapshot_dir);
        // Only inside a volume that really is mounted. After a failed mount
        // the path is a bare directory, and a snapshot dir created there is a
        // write to whatever filesystem lies underneath. An empty or relative
        // `volume` resolves against the working directory instead — which is
        // how stray root-owned `.btrbk-snapshots` directories once appeared
        // in whatever directory the job was started from.
        if !volume.is_absolute() || !volume.exists() || !(probes.is_mountpoint)(volume) {
            progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!(
                    "Source '{}': snapshot dir {} not created — volume '{}' is not mounted",
                    source.label,
                    snap_dir.display(),
                    source.volume
                ),
            );
            continue;
        }
        if !snap_dir.exists()
            && let Err(e) = std::fs::create_dir_all(&snap_dir)
        {
            // Previously `let _ = Command::new("mkdir")…` — a read-only or full
            // filesystem produced no signal at all, and btrbk failed later with
            // a generic "directory not found" (bd DAS-Backup-Manager-06p).
            progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!("Could not create snapshot dir {}: {e}", snap_dir.display()),
            );
        }
    }

    // Create target subdirectories on mounted targets (btrbk expects them).
    for target in &config.targets {
        let target_path = Path::new(&target.mount);
        if !target_path.exists() || !(probes.is_mountpoint)(target_path) {
            continue;
        }
        for source in &config.sources {
            for subdir in &source.target_subdirs {
                let dir = target_path.join(subdir);
                if !dir.exists()
                    && let Err(e) = std::fs::create_dir_all(&dir)
                {
                    progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!("Could not create target subdir {}: {e}", dir.display()),
                    );
                }
            }
        }
    }

    guard
}

/// Create the directory each write target receives into, as
/// `render_btrbk_conf` names it — after the targets are mounted and verified
/// and after sync reloaded the config, so a source sync just adopted (a new
/// `<source>-adopted`) gets its directory on this run, as `create_target_dirs`
/// does in `backup-run.sh` (bd DAS-Backup-Manager-arx). Only under a target
/// that is a mount point; a subdirectory that is empty, absolute or climbs
/// with `..` is refused. Each directory created is logged. `Err` lists every
/// directory that could not be made: btrbk would skip that target, so the
/// run must say it failed.
///
/// Only the first `target_subdirs` entry of a source is created, because it
/// is the only one the renderer uses; `backup-run.sh` creates every entry.
pub fn create_target_dirs(
    config: &Config,
    write_labels: &[String],
    progress: &dyn ProgressCallback,
) -> Result<(), String> {
    create_target_dirs_with(config, write_labels, progress, &HOST_PROBES)
}

/// [`create_target_dirs`] against an explicit host.
fn create_target_dirs_with(
    config: &Config,
    write_labels: &[String],
    progress: &dyn ProgressCallback,
    probes: &MountProbes<'_>,
) -> Result<(), String> {
    let mut failures = Vec::new();
    for (target, subdir) in crate::btrbk_conf::target_dirs(config) {
        if !write_labels.contains(&target.label) {
            continue;
        }
        let relative = Path::new(subdir);
        let plain = !subdir.is_empty()
            && relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !plain {
            failures.push(format!(
                "{}: target subdirectory '{subdir}' is not a plain relative path — not created",
                target.label
            ));
            continue;
        }
        let mount = Path::new(&target.mount);
        if !(probes.is_mountpoint)(mount) {
            failures.push(format!(
                "{}: '{}' is not a mount point — '{subdir}' not created there",
                target.label, target.mount
            ));
            continue;
        }
        let dir = mount.join(relative);
        if dir.is_dir() {
            continue;
        }
        match std::fs::create_dir_all(&dir) {
            Ok(()) => progress.on_log(
                crate::progress::LogLevel::Info,
                &format!("Target '{}': created {}", target.label, dir.display()),
            ),
            Err(e) => failures.push(format!(
                "{}: could not create {} ({e})",
                target.label,
                dir.display()
            )),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Target directories missing — btrbk will skip these targets:\n  - {}",
            failures.join("\n  - ")
        ))
    }
}

/// The BTRFS filesystem UUID of whatever is mounted at `path`, if anything.
///
/// Shells out to `findmnt` rather than reading the superblock so the answer is
/// about the MOUNT, not about a device we hope is mounted there. Returns `None`
/// when nothing is mounted at `path` or `findmnt` is unavailable.
pub fn filesystem_uuid_at(path: &str) -> Option<String> {
    filesystem_uuid_with(&SystemRunner, path)
}

/// [`filesystem_uuid_at`] with the `findmnt` invocation handed to `runner`.
fn filesystem_uuid_with(runner: &dyn CommandRunner, path: &str) -> Option<String> {
    let out = runner
        .output(Command::new("findmnt").args(["-n", "-o", "UUID", "--target", path]))
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uuid.is_empty() { None } else { Some(uuid) }
}

/// The device the filesystem mounted at `mount` is on: `findmnt`'s `SOURCE`
/// without the `[/subvolume]` it appends for btrfs. `None` when `findmnt`
/// cannot say, or says something that is not a device (`proc`, `tmpfs`).
fn mount_source(runner: &dyn CommandRunner, mount: &str) -> Option<String> {
    let out = runner
        .output(Command::new("findmnt").args(["-n", "-o", "SOURCE", "--target", mount]))
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let source = text.lines().next()?.split('[').next()?.trim();
    source.starts_with("/dev/").then(|| source.to_string())
}

/// The disk a partition is on (`lsblk -dno PKNAME`); a device with no parent
/// is a whole disk and is its own. `None` when `lsblk` cannot answer.
fn disk_of(runner: &dyn CommandRunner, device: &str) -> Option<String> {
    let out = runner
        .output(Command::new("lsblk").args(["-dno", "PKNAME", device]))
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let parent = text.lines().next().unwrap_or_default().trim();
    Some(if parent.is_empty() {
        device.to_string()
    } else {
        format!("/dev/{parent}")
    })
}

/// The serial number `smartctl -i` reports for `disk`. Its exit status is a
/// bitmask of what it found, not of whether it could read the drive, so only
/// its text says: no `Serial Number:` line is `None`.
fn disk_serial(runner: &dyn CommandRunner, disk: &str) -> Option<String> {
    let out = runner
        .output(Command::new("smartctl").args(["-i", disk]))
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("Serial Number:"))
        .map(|serial| serial.trim().to_string())
        .filter(|serial| !serial.is_empty())
}

/// Whether the drive under `target`'s mount point is one of the drives its
/// config names — `verify_targets_before_btrbk`'s legacy check in
/// `scripts/backup-run.sh`, for a target with no `mount_uuid`: the device under
/// the mount, its disk, that disk's serial, compared with the target's serials
/// (a RAID-1 pair has two; either is enough). `Ok` says what was found;
/// `Err` is the violation. A serial that cannot be read is a refusal, never a
/// pass: nothing was shown to be the drive, so nothing is let through (bd
/// DAS-Backup-Manager-7tx 4).
fn verify_legacy_serial(
    runner: &dyn CommandRunner,
    target: &crate::config::Target,
) -> Result<String, String> {
    let expected = target.effective_serials();
    if expected.is_empty() {
        return Err(format!(
            "{}: '{}' has no mount_uuid and no serial in the config, so nothing says which \
             filesystem belongs there",
            target.label, target.mount
        ));
    }
    let Some(source) = mount_source(runner, &target.mount) else {
        return Err(format!(
            "{}: '{}' is a mount point but findmnt could not resolve its source device",
            target.label, target.mount
        ));
    };
    let serial = disk_of(runner, &source).and_then(|disk| disk_serial(runner, &disk));
    match serial {
        Some(got) if expected.contains(&got) => {
            Ok(format!("{} → {source}, serial={got}", target.mount))
        }
        other => Err(format!(
            "{}: '{}' mounted from {source} (serial='{}'), expected one of: {}",
            target.label,
            target.mount,
            other.as_deref().unwrap_or("?"),
            expected.join(" ")
        )),
    }
}

/// Refuse to hand btrbk a target that is not backed by the filesystem we expect.
///
/// This is the Rust counterpart of `verify_targets_before_btrbk()` in
/// `scripts/backup-run.sh`, and it exists for the same reason: writing to a
/// **bare mount point** falls through to the underlying filesystem — normally
/// the NVMe root — and fills it. That is not hypothetical; it happened in May
/// 2026 when the original 22 TB drive was removed and a backup ran before the
/// replacement was in place (bd DAS-Backup-Manager-9on).
///
/// The bash path grew this guard; the Rust path never did, so the incident
/// stayed reachable from every caller that pre-mounts and then names its
/// targets explicitly — which the Plasma GUI always does
/// (bd DAS-Backup-Manager-aea).
///
/// A write target is identified by its `mount_uuid` when it has one, else by its
/// drive's serial number (`verify_legacy_serial`), as the script does: both
/// refuse when the identity cannot be read.
///
/// Scope is deliberate: only targets btrbk will actually be told to write to
/// are fatal. A configured target that is not part of this run cannot be
/// written to, so a stale directory there is reported as a warning (it is
/// evidence of a past leak worth looking at) but does not abort the run.
pub fn verify_write_targets(
    targets: &[crate::config::Target],
    write_labels: &[String],
    progress: &dyn ProgressCallback,
) -> Result<(), String> {
    verify_write_targets_with(targets, write_labels, progress, &HOST_PROBES, &SystemRunner)
}

/// [`verify_write_targets`] against an explicit host.
fn verify_write_targets_with(
    targets: &[crate::config::Target],
    write_labels: &[String],
    progress: &dyn ProgressCallback,
    probes: &MountProbes<'_>,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    let mut violations: Vec<String> = Vec::new();

    for target in targets {
        let will_write = write_labels.iter().any(|l| l == &target.label);
        let mount_path = Path::new(&target.mount);

        if !will_write {
            // Not a write target this run. A bare non-empty directory here is
            // not dangerous now, but it is how 9on presented, so say so.
            if mount_path.exists() && !(probes.is_mountpoint)(mount_path) {
                match std::fs::read_dir(mount_path) {
                    Ok(mut d) => {
                        if d.next().is_some() {
                            progress.on_log(
                                crate::progress::LogLevel::Warning,
                                &format!(
                                    "Target '{}' is not part of this run, but '{}' is a non-empty \
                                     bare directory — a previous run may have written to it \
                                     (bd DAS-Backup-Manager-9on)",
                                    target.label, target.mount
                                ),
                            );
                        }
                    }
                    // `.unwrap_or(false)` here read "could not look" as "empty,
                    // nothing to see", which is the one reading this check
                    // exists to rule out — a plain FILE sitting at the mount
                    // path fails read_dir with ENOTDIR and used to pass in
                    // total silence (bd DAS-Backup-Manager-8wx).
                    Err(e) => progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!(
                            "Target '{}' is not part of this run and '{}' exists but could not \
                             be listed ({e}) — cannot rule out a previous run having written \
                             to it (bd DAS-Backup-Manager-9on)",
                            target.label, target.mount
                        ),
                    ),
                }
            }
            continue;
        }

        // A write target MUST be a real mount point.
        if !(probes.is_mountpoint)(mount_path) {
            violations.push(format!(
                "{} ({:?}): expected mounted but '{}' is NOT a mount point — writing here \
                 would fill the underlying filesystem",
                target.label, target.role, target.mount
            ));
            continue;
        }

        // And it must be the filesystem we expect, not merely *a* filesystem.
        // UUID is preferred: it still matches when a BTRFS RAID-1 array is
        // mounted degraded from a single leg, which a device check would not.
        if let Some(expected) = target.mount_uuid.as_deref().filter(|u| !u.is_empty()) {
            match filesystem_uuid_with(runner, &target.mount) {
                Some(actual) if actual == expected && raid_guard::is_guarded(target) => {
                    match raid_member_check(target, expected, probes) {
                        Ok(note) => progress.on_log(
                            if note.is_some() {
                                crate::progress::LogLevel::Warning
                            } else {
                                crate::progress::LogLevel::Info
                            },
                            &format!(
                                "  {}: OK ({} → UUID={}){}",
                                target.label,
                                target.mount,
                                actual,
                                note.map(|n| format!(" — {n}")).unwrap_or_default()
                            ),
                        ),
                        Err(violation) => violations.push(violation),
                    }
                }
                Some(actual) if actual == expected => {
                    progress.on_log(
                        crate::progress::LogLevel::Info,
                        &format!(
                            "  {}: OK ({} → UUID={})",
                            target.label, target.mount, actual
                        ),
                    );
                }
                Some(actual) => violations.push(format!(
                    "{}: '{}' has filesystem UUID '{}', expected '{}' — a different \
                     filesystem is mounted here",
                    target.label, target.mount, actual, expected
                )),
                None => violations.push(format!(
                    "{}: could not determine the filesystem UUID mounted at '{}' \
                     (expected '{}')",
                    target.label, target.mount, expected
                )),
            }
        } else {
            // No UUID configured (legacy target): the drive's serial number is
            // the identity, as `verify_targets_before_btrbk` has it.
            match verify_legacy_serial(runner, target) {
                Ok(found) => progress.on_log(
                    crate::progress::LogLevel::Info,
                    &format!("  {}: OK ({found})", target.label),
                ),
                Err(violation) => violations.push(violation),
            }
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Refusing to run btrbk — backup target verification failed:\n  - {}",
            violations.join("\n  - ")
        ))
    }
}

/// Step 7 of the RAID-1 guard (bd DAS-Backup-Manager-su1b.10), for a mount
/// made by anyone: the kernel reporting a member missing while every
/// configured drive is attached is the 2026-11-21 incident — refused. A
/// member missing with a drive truly absent is a degraded mount, said
/// (`Ok(Some)`). Unreadable is refused.
fn raid_member_check(
    target: &Target,
    fsid: &str,
    probes: &MountProbes<'_>,
) -> Result<Option<String>, String> {
    let missing = (probes.missing_devids)(fsid).map_err(|e| {
        format!(
            "{}: cannot tell whether a member of its RAID-1 is missing ({e})",
            target.label
        )
    })?;
    if missing.is_empty() {
        return Ok(None);
    }
    let serials = target.effective_serials();
    let absent: Vec<&String> = serials
        .iter()
        .filter(|s| (probes.device_from_serial)(s).is_none())
        .collect();
    if absent.is_empty() {
        return Err(format!(
            "{}: mounted with devid(s) {missing:?} missing while every configured drive ({}) is \
             attached — a degraded mount nothing guarded (bd DAS-Backup-Manager-su1b.10)",
            target.label,
            serials.join(", ")
        ));
    }
    Ok(Some(format!(
        "RAID-1 DEGRADED: devid(s) {missing:?} missing, drive(s) {} absent",
        absent
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Where a recovery drive's ESP is mounted while it is read: one directory
/// per target under this one (bd DAS-Backup-Manager-sp78.20).
pub const ESP_MOUNT_ROOT: &str = "/run/das-backup/esp";

/// The one mount this project makes of an ESP (bd DAS-Backup-Manager-sp78.20):
/// a mirror target's own ESP, the partition its `esp_partuuid` names, mounted
/// read-only at [`ESP_MOUNT_ROOT`]`/<label>` for `read`, then unmounted.
/// `drive_ids` are the names `lsblk` gives this drive's partitions
/// (`PARTUUID=…` among them). [`EspReading::read`] is what `read` found, or
/// why the ESP was not read; [`EspReading::left_mounted`] says why it is still
/// mounted, when it could not be taken down afterwards (bd s5jv.2).
///
/// The configured PARTUUID is a claim, not an identity: it is mounted only
/// when the drive itself lists it, so a mistyped or copied key — the host's
/// `EFI` — is refused before anything is asked of the host. Never
/// discovered (`.claude/rules/esp-safety.md`). Mounted at another place by
/// another program (udisks, a unit, by hand) it is cleared by the stop policy
/// as a job that writes nothing clears it (bd DAS-Backup-Manager-eg4j.19): an
/// idle mount is unmounted and the reading goes on; one something uses is not
/// read, the user named, nothing signalled ([`ESP_HELD`]). Mounted at its own
/// directory, an earlier reading left it, and it is taken down as a target an
/// earlier job left is (bd jgm).
///
/// `report_only` (a dry run, review N2): another program's mount is said
/// ("would unmount", "would stop"), never taken down, and the ESP is then
/// not read.
pub fn read_recovery_esp<T>(
    target: &crate::config::Target,
    drive_ids: &std::collections::BTreeSet<String>,
    report_only: bool,
    read: &dyn Fn(&Path) -> Result<T, String>,
) -> EspReading<T> {
    read_recovery_esp_with(
        &SystemRunner,
        Path::new(ESP_MOUNT_ROOT),
        &host_clear_site(report_only, false),
        Path::new("/dev/disk/by-partuuid"),
        target,
        drive_ids,
        read,
    )
}

/// What a "not read" ESP's text says when another program's mount of it was
/// held by something: a finding of the reading (a WARN row), not a missing
/// setting (bd DAS-Backup-Manager-eg4j.19).
pub const ESP_HELD: &str = "held by another program";

/// A reading of a recovery drive's ESP ([`read_recovery_esp`]): what `read`
/// said, or why it was not asked, and — when the ESP could not be taken down
/// after it — why it is still mounted. A failed unmount never takes the
/// reading's place (bd DAS-Backup-Manager-s5jv.2).
#[derive(Debug, PartialEq)]
pub struct EspReading<T> {
    pub read: Result<T, String>,
    pub left_mounted: Option<String>,
}

fn read_recovery_esp_with<T>(
    runner: &dyn CommandRunner,
    root: &Path,
    clear: &ClearSite<'_>,
    partuuid_dir: &Path,
    target: &crate::config::Target,
    drive_ids: &std::collections::BTreeSet<String>,
    read: &dyn Fn(&Path) -> Result<T, String>,
) -> EspReading<T> {
    let mut left_mounted = None;
    let read = mount_read_unmount(
        runner,
        root,
        clear,
        partuuid_dir,
        target,
        drive_ids,
        read,
        &mut left_mounted,
    );
    EspReading { read, left_mounted }
}

/// [`read_recovery_esp_with`]'s steps: `Err` when the ESP was not read;
/// `left_mounted` is set when, read or not, it is still mounted at the end.
#[allow(clippy::too_many_arguments)] // one reading's whole host, passed as it is
fn mount_read_unmount<T>(
    runner: &dyn CommandRunner,
    root: &Path,
    clear: &ClearSite<'_>,
    partuuid_dir: &Path,
    target: &crate::config::Target,
    drive_ids: &std::collections::BTreeSet<String>,
    read: &dyn Fn(&Path) -> Result<T, String>,
    left_mounted: &mut Option<String>,
) -> Result<T, String> {
    let pu = target
        .esp_partuuid
        .as_deref()
        .ok_or("no `esp_partuuid` is configured for this target")?;
    // lsblk and libblkid write a PARTUUID in lower case; a tag lookup is
    // exact, so the key is asked for as they write it.
    let spec = format!("PARTUUID={}", pu.to_ascii_lowercase());
    let listed = crate::config::is_partuuid(pu)
        && drive_ids.iter().any(|id| {
            id.split_once('=')
                .is_some_and(|(k, v)| k == "PARTUUID" && v.eq_ignore_ascii_case(pu))
        });
    if !listed {
        return Err(format!(
            "{spec} is not a partition of this drive — not mounted"
        ));
    }
    // The ESP's device is the one the drive's own listing pairs with the
    // PARTUUID (review B1): another attached partition may carry the same
    // PARTUUID (a cloned GPT), and `/dev/disk/by-partuuid` may then point
    // at it — the host's `/boot`, say.
    let drive_dev =
        drive_esp_dev(drive_ids, pu).map_err(|why| format!("{spec}: {why} — not read"))?;
    let part = partuuid_dir.join(pu.to_ascii_lowercase());
    let part_s = part.to_str().ok_or("the ESP's device path is not UTF-8")?;
    // `by-partuuid` must be that device, before anything is cleared and
    // again before the mount (`where_` names where it was found mounted).
    let is_drive_s = |where_: &str| -> Result<(), String> {
        let dev =
            (clear.block_dev)(part_s).map_err(|e| format!("{spec}{where_}: {e} — not read"))?;
        if majmin(&dev) == Some(drive_dev) {
            Ok(())
        } else {
            Err(format!(
                "{spec}{where_}: {part_s} is device {dev}, but this drive's partition with that \
                 PARTUUID is {}:{} — another partition carries the same PARTUUID; not read",
                drive_dev.0, drive_dev.1
            ))
        }
    };
    let dir = root.join(esp_dir_name(&target.label));
    let dir_s = dir
        .to_str()
        .ok_or("the ESP's mount directory is not UTF-8")?;

    // Where that partition is mounted now: findmnt exits 1 when nowhere.
    let out = runner
        .output(Command::new("findmnt").args(["-n", "-o", "TARGET", "-S", &spec]))
        .map_err(|e| format!("findmnt failed to run: {e}"))?;
    match out.status.code() {
        Some(0) => {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if let Some(there) = text.lines().find(|there| *there != dir_s) {
                // Another program's mount (bd DAS-Backup-Manager-eg4j.19):
                // by the stop policy of a job that writes nothing. The
                // partition by the drive's own device number for it, once
                // by-partuuid is shown to be that device (review B1).
                let held = |why: &str| {
                    format!("{spec} is mounted at {there}: {why} — not read ({ESP_HELD})")
                };
                is_drive_s(&format!(" is mounted at {there}"))?;
                let id = crate::holders::PartitionId {
                    path: part_s.to_string(),
                    dev: format!("{}:{}", drive_dev.0, drive_dev.1),
                };
                match clear_partitions_with(
                    runner,
                    clear,
                    &target.label,
                    &[id],
                    None,
                    &crate::progress::NullProgress,
                )
                .map_err(|why| held(&why))?
                {
                    // findmnt saw the PARTUUID mounted there, yet no mount of
                    // the drive's partition is: it is another partition's,
                    // and the ESP is never mounted beside it (review B1).
                    None => {
                        return Err(format!(
                            "{spec} is mounted at {there}, but no mount of this drive's partition \
                             ({}:{}) is — another partition carries the same PARTUUID; not read",
                            drive_dev.0, drive_dev.1
                        ));
                    }
                    // A dry run (review N2): what a real reading would do
                    // is said, nothing is taken down, and the ESP is never
                    // read beside another program's mount.
                    Some(would) if clear.report_only => {
                        return Err(format!(
                            "{spec} is mounted at {there}: a dry run takes nothing down ({}) — \
                             not read",
                            would.join("; ")
                        ));
                    }
                    Some(_) => {}
                }
            } else if clear.report_only {
                // A dry run changes nothing (review N2): not even a mount an
                // earlier reading left at its own directory.
                return Err(format!(
                    "{spec} was left mounted at {dir_s}: a dry run takes nothing down (would \
                     unmount {dir_s}) — not read"
                ));
            } else {
                take_down_leftover_with(runner, dir_s, &crate::progress::NullProgress, None)
                    .map_err(|why| format!("{spec} was left mounted at {dir_s}: {why}"))?;
            }
        }
        Some(1) => {}
        _ => return Err(exit_why("findmnt", out.status)),
    }

    is_drive_s("")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{dir_s}: {e}"))?;
    let gone = |r: Result<T, String>| {
        // Empty once unmounted; a directory left behind is only clutter.
        let _ = std::fs::remove_dir(&dir);
        r
    };
    // 32: not a mount point (util-linux); anything else is no answer.
    match runner.status(Command::new("mountpoint").args(["-q", dir_s])) {
        Ok(s) if s.code() == Some(32) => {}
        Ok(s) if s.success() => {
            return Err(format!(
                "{dir_s} is already a mount point — not mounted over"
            ));
        }
        Ok(s) => return gone(Err(exit_why("mountpoint", s))),
        Err(e) => return gone(Err(format!("mountpoint failed to run: {e}"))),
    }
    match runner.status(Command::new("mount").args([
        "-t",
        "vfat",
        "-o",
        "ro,nodev,nosuid,noexec,noatime",
        part_s,
        dir_s,
    ])) {
        Ok(s) if s.success() => {}
        Ok(s) => return gone(Err(exit_why("mount", s))),
        Err(e) => return gone(Err(format!("mount failed to run: {e}"))),
    }

    // Read only what is shown to be that partition: its PARTUUID and the
    // drive's device number for it (review B1: the link may have moved
    // between the check and the mount).
    let found = runner
        .output(Command::new("findmnt").args([
            "-n",
            "-P",
            "-o",
            "PARTUUID,MAJ:MIN",
            "--mountpoint",
            dir_s,
        ]))
        .ok()
        .filter(|o| o.status.success())
        .map(|o| pairs(&String::from_utf8_lossy(&o.stdout)));
    let read = match found {
        Some((Some(f), _)) if !f.eq_ignore_ascii_case(pu) => {
            Err(format!("{dir_s} holds PARTUUID={f}, not {spec} — not read"))
        }
        Some((Some(_), Some(d))) if majmin(&d) == Some(drive_dev) => read(&dir),
        Some((Some(_), Some(d))) => Err(format!(
            "{dir_s} holds device {d}, not this drive's partition {}:{} — not read",
            drive_dev.0, drive_dev.1
        )),
        _ => Err(format!(
            "what is mounted at {dir_s} could not be told — not read"
        )),
    };
    match umount_retrying(runner, dir_s, |_| {}) {
        Ok(()) => gone(read),
        Err(why) => {
            *left_mounted = Some(format!(
                "{spec} is still mounted at {dir_s} after {UMOUNT_ATTEMPTS} attempts ({why}); \
                 the next reading takes it down"
            ));
            read
        }
    }
}

/// The device number the drive's own listing pairs with PARTUUID `pu`
/// ([`crate::recovery_os::PARTUUID_DEV`]); `Err` when it pairs none, or more
/// than one.
fn drive_esp_dev(
    drive_ids: &std::collections::BTreeSet<String>,
    pu: &str,
) -> Result<(u32, u32), String> {
    let mut devs: Vec<(u32, u32)> = drive_ids
        .iter()
        .filter_map(|id| id.split_once('='))
        .filter(|(k, _)| *k == crate::recovery_os::PARTUUID_DEV)
        .filter_map(|(_, v)| v.rsplit_once('@'))
        .filter(|(p, _)| p.eq_ignore_ascii_case(pu))
        .map(|(_, d)| majmin(d).ok_or_else(|| format!("its device number '{d}' cannot be read")))
        .collect::<Result<_, _>>()?;
    devs.sort_unstable();
    devs.dedup();
    match devs.as_slice() {
        [one] => Ok(*one),
        [] => Err("the drive's listing pairs no device number with it".into()),
        _ => Err("the drive's listing pairs more than one device number with it".into()),
    }
}

/// `major:minor` as numbers; `None` for anything else.
fn majmin(s: &str) -> Option<(u32, u32)> {
    let (maj, min) = s.trim().split_once(':')?;
    Some((maj.parse().ok()?, min.parse().ok()?))
}

/// The `PARTUUID` and `MAJ:MIN` (or `MAJ_MIN`) values of `findmnt -P`
/// output's first line; an empty value is none.
fn pairs(text: &str) -> (Option<String>, Option<String>) {
    let (mut pu, mut dev) = (None, None);
    for pair in text.lines().next().unwrap_or("").split("\" ") {
        let pair = pair.trim().trim_end_matches('"');
        if let Some((key, value)) = pair.split_once("=\"")
            && !value.is_empty()
        {
            match key {
                "PARTUUID" => pu = Some(value.to_string()),
                "MAJ:MIN" | "MAJ_MIN" => dev = Some(value.to_string()),
                _ => {}
            }
        }
    }
    (pu, dev)
}

/// A target's label as one safe file name under [`ESP_MOUNT_ROOT`].
fn esp_dir_name(label: &str) -> String {
    let name: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() || name.chars().all(|c| c == '.') {
        format!("_{name}")
    } else {
        name
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use crate::config::{Retention, Source, Target};
    use crate::progress::{LogLevel, TestProgress};

    // -----------------------------------------------------------------------
    // Scripted host: nothing below ever spawns mount(8) or umount(8).
    // -----------------------------------------------------------------------

    /// What a scripted command does when it is "run".
    #[derive(Clone, Debug)]
    enum Reply {
        Exit {
            code: i32,
            stdout: &'static str,
            stderr: &'static str,
        },
        /// The program could not be spawned at all.
        CannotRun,
        /// Exits 32 having left a file at this path — a mount that failed
        /// but did not leave its mount point as it found it.
        FailLeavingFile(String),
        /// Exits with these codes on successive calls, then the last one for
        /// every call after.
        Seq(&'static [i32]),
        /// Exits 32, yet the mount took effect: a mount helper that failed
        /// after the kernel had mounted (bd DAS-Backup-Manager-1fpw).
        FailMounting,
        /// Exits with this code, printing this (built at run time: a path
        /// under the test's scratch directory).
        Out(i32, String),
    }

    impl Reply {
        fn ok() -> Self {
            Self::exit(0)
        }

        fn exit(code: i32) -> Self {
            Self::Exit {
                code,
                stdout: "",
                stderr: "",
            }
        }
    }

    /// One command handed to the runner: `captured` is true when it came in
    /// through `output()`, false for `status()`.
    #[derive(Debug, Clone, PartialEq)]
    struct Call {
        captured: bool,
        argv: Vec<String>,
    }

    fn status_call(argv: &[&str]) -> Call {
        Call {
            captured: false,
            argv: argv.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn output_call(argv: &[&str]) -> Call {
        Call {
            captured: true,
            argv: argv.iter().map(|a| a.to_string()).collect(),
        }
    }

    /// Records every command and answers from a script instead of running it.
    /// A rule applies to any command with an argument equal to its key; a
    /// command no rule matches succeeds silently.
    struct ScriptedRunner {
        calls: Mutex<Vec<Call>>,
        rules: Vec<(String, Reply)>,
        /// Mount points of every scripted `mount` that succeeded and has not
        /// been `umount`ed since, so the host's mount table can follow.
        mounted: Mutex<Vec<String>>,
        /// Mount points a scripted `umount` released, so a mount the host
        /// described as pre-existing stops being one (bd jgm).
        unmounted: Mutex<Vec<String>>,
        /// How many times each `Seq` rule has answered.
        seq_pos: Mutex<std::collections::HashMap<String, usize>>,
        /// Every pause asked for, in order; none is really taken.
        pauses: Mutex<Vec<Duration>>,
        /// How many commands had been run when each pause was asked for
        /// (parallel to `pauses`): where in the sequence it fell.
        pause_at: Mutex<Vec<usize>>,
    }

    impl ScriptedRunner {
        fn succeeding() -> Arc<Self> {
            Self::with_rules(&[])
        }

        fn with_rules(rules: &[(&str, Reply)]) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                rules: rules
                    .iter()
                    .map(|(key, reply)| (key.to_string(), reply.clone()))
                    .collect(),
                mounted: Mutex::new(Vec::new()),
                unmounted: Mutex::new(Vec::new()),
                seq_pos: Mutex::new(std::collections::HashMap::new()),
                pauses: Mutex::new(Vec::new()),
                pause_at: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn run(&self, captured: bool, cmd: &Command) -> io::Result<Output> {
            let mut argv = vec![cmd.get_program().to_string_lossy().into_owned()];
            argv.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
            let reply = match self
                .rules
                .iter()
                .find(|(key, _)| argv.iter().any(|a| a == key))
            {
                Some((key, Reply::Seq(codes))) => {
                    let mut pos = self.seq_pos.lock().unwrap();
                    let n = pos.entry(key.clone()).or_insert(0);
                    let code = codes[(*n).min(codes.len() - 1)];
                    *n += 1;
                    Reply::exit(code)
                }
                Some((_, reply)) => reply.clone(),
                None => Reply::ok(),
            };
            if (matches!(reply, Reply::Exit { code: 0, .. } | Reply::Out(0, _))
                || (matches!(reply, Reply::FailMounting) && argv[0] == "mount"))
                && let Some(mount_point) = argv.last()
            {
                let mut mounted = self.mounted.lock().unwrap();
                match argv[0].as_str() {
                    "mount" => mounted.push(mount_point.clone()),
                    "umount" => {
                        mounted.retain(|m| m != mount_point);
                        self.unmounted.lock().unwrap().push(mount_point.clone());
                    }
                    _ => {}
                }
            }
            self.calls.lock().unwrap().push(Call { captured, argv });
            match reply {
                Reply::Exit {
                    code,
                    stdout,
                    stderr,
                } => Ok(Output {
                    // wait(2) encoding: the exit code lives in the high byte.
                    status: ExitStatus::from_raw(code << 8),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: stderr.as_bytes().to_vec(),
                }),
                Reply::Out(code, stdout) => Ok(Output {
                    status: ExitStatus::from_raw(code << 8),
                    stdout: stdout.into_bytes(),
                    stderr: Vec::new(),
                }),
                Reply::Seq(_) => unreachable!("resolved to an exit above"),
                Reply::CannotRun => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "scripted: no such program",
                )),
                Reply::FailMounting => Ok(Output {
                    status: ExitStatus::from_raw(32 << 8),
                    stdout: Vec::new(),
                    stderr: b"mount: helper failed after mounting".to_vec(),
                }),
                Reply::FailLeavingFile(path) => {
                    std::fs::write(&path, b"left behind").unwrap();
                    Ok(Output {
                        status: ExitStatus::from_raw(32 << 8),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    })
                }
            }
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn status(&self, cmd: &mut Command) -> io::Result<ExitStatus> {
            self.run(false, cmd).map(|o| o.status)
        }

        fn output(&self, cmd: &mut Command) -> io::Result<Output> {
            self.run(true, cmd)
        }

        fn pause(&self, how_long: Duration) {
            self.pause_at
                .lock()
                .unwrap()
                .push(self.calls.lock().unwrap().len());
            self.pauses.lock().unwrap().push(how_long);
        }
    }

    /// A described host: which paths are mount points, which drives are
    /// present, and which are already mounted somewhere other than their
    /// configured path. A path also counts as a mount point while a scripted
    /// `mount` onto it has succeeded and not been undone.
    #[derive(Default)]
    struct Host {
        mountpoints: Vec<PathBuf>,
        /// serial → where that drive's partition is currently mounted
        mounted_elsewhere: Vec<(String, String)>,
        /// serial → whole-disk device path
        devices: Vec<(String, String)>,
        /// filesystem UUID → a device holding it (`blkid -U`)
        uuids: Vec<(String, String)>,
        /// filesystem UUID → the devids the kernel marks missing
        missing: Vec<(String, Vec<u64>)>,
        /// What the kernel shows missing when the job ends, per filesystem
        /// UUID (none listed: nothing).
        missing_at_end: Vec<(String, Vec<u64>)>,
        /// Where a guarded mount keeps its record; never the host's.
        raid_dir: Option<PathBuf>,
        /// The intent the targets are mounted with (none: a writing job).
        intent: Option<raid_guard::Intent>,
        /// `/proc/self/mountinfo` lines (bd su1b.12); a line whose mount point
        /// a scripted `umount` released is gone from it.
        mountinfo: Vec<String>,
        /// The process table (none: an empty one).
        proc: Option<Arc<crate::holders::testproc::FakeProc>>,
        /// Pids a signal does not end.
        survive: Vec<i32>,
        /// Every signal sent, in order.
        signals: Mutex<Vec<(i32, crate::holders::Sig)>>,
        /// Filesystem UUIDs `/sys/fs/btrfs` lists (mounted somewhere).
        in_sysfs: Vec<String>,
        /// `/sys/fs/btrfs/<fsid>/exclusive_operation`, per filesystem UUID.
        exclusive: Vec<(String, String)>,
        /// Clear as a dry run does.
        report_only: bool,
        /// Clear for a job that writes nothing under the target: it may
        /// not stop readers (operator 2026-11-25).
        read_only_job: bool,
        /// path → `major:minor` of the block devices that exist (none
        /// listed: the path is not a block device).
        block_devs: Vec<(String, String)>,
        /// What the mount table shows from its second reading on (none:
        /// the same as ever): another program mounting over a path between
        /// the scan and the unmount.
        mountinfo_later: Option<Vec<String>>,
    }

    /// The described host's signals: recorded, and the process ends unless
    /// the host says it survives.
    struct HostSignals<'a>(&'a Host);

    impl crate::holders::Signaller for HostSignals<'_> {
        fn signal(&self, pid: i32, _start: u64, sig: crate::holders::Sig) -> Result<(), String> {
            assert!(pid > 1, "a signal to pid {pid}");
            self.0.signals.lock().unwrap().push((pid, sig));
            if !self.0.survive.contains(&pid)
                && let Some(p) = &self.0.proc
            {
                p.remove(pid);
            }
            Ok(())
        }
    }

    impl Host {
        fn signals(&self) -> Vec<(i32, crate::holders::Sig)> {
            self.signals.lock().unwrap().clone()
        }

        /// What clearing reads and does on this described host.
        fn with_clear<R>(&self, runner: &ScriptedRunner, f: impl FnOnce(&ClearSite<'_>) -> R) -> R {
            let empty = crate::holders::testproc::FakeProc::new();
            let site = self.proc.as_deref().unwrap_or(&empty).site();
            let sysfs = tempfile::TempDir::new().unwrap();
            for u in &self.in_sysfs {
                std::fs::create_dir_all(sysfs.path().join(u)).unwrap();
            }
            for (u, op) in &self.exclusive {
                std::fs::create_dir_all(sysfs.path().join(u)).unwrap();
                std::fs::write(sysfs.path().join(u).join("exclusive_operation"), op).unwrap();
            }
            let reads = std::cell::Cell::new(0usize);
            let mountinfo = || {
                let gone = runner.unmounted.lock().unwrap().clone();
                reads.set(reads.get() + 1);
                let table = match (&self.mountinfo_later, reads.get()) {
                    (Some(later), n) if n >= 2 => later,
                    _ => &self.mountinfo,
                };
                Ok(table
                    .iter()
                    .filter(|l| !gone.iter().any(|g| l.split(' ').nth(4) == Some(g)))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"))
            };
            let signals = HostSignals(self);
            let block_dev = |p: &str| {
                self.block_devs
                    .iter()
                    .find(|(path, _)| path == p)
                    .map(|(_, d)| d.clone())
                    .ok_or_else(|| format!("{p} is not a block device"))
            };
            f(&ClearSite {
                block_dev: &block_dev,
                mountinfo: &mountinfo,
                proc: site,
                signaller: &signals,
                self_pid: 900,
                report_only: self.report_only,
                stop_readers: !self.read_only_job,
                sysfs_btrfs: sysfs.path().to_path_buf(),
            })
        }

        fn with_probes<R>(
            &self,
            runner: &ScriptedRunner,
            f: impl FnOnce(&MountProbes<'_>) -> R,
        ) -> R {
            let is_mountpoint = |p: &Path| {
                (self.mountpoints.iter().any(|m| m == p)
                    && !runner
                        .unmounted
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|m| Path::new(m) == p))
                    || runner
                        .mounted
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|m| Path::new(m) == p)
            };
            let find_mount_for_device = |serial: &str, _role: &TargetRole| {
                self.mounted_elsewhere
                    .iter()
                    .find(|(s, _)| s == serial)
                    .map(|(_, at)| at.clone())
            };
            let device_from_serial = |serial: &str| {
                self.devices
                    .iter()
                    .find(|(s, _)| s == serial)
                    .map(|(_, dev)| dev.clone())
            };
            let device_for_uuid = |uuid: &str| {
                self.uuids
                    .iter()
                    .find(|(u, _)| u == uuid)
                    .map(|(_, dev)| dev.clone())
            };
            let missing_devids = |uuid: &str| {
                Ok(self
                    .missing
                    .iter()
                    .find(|(u, _)| u == uuid)
                    .map(|(_, m)| m.clone())
                    .unwrap_or_default())
            };
            // A partition sits on the attached drive whose whole-disk device
            // path it starts with (`dev/sdb1` on `dev/sdb`).
            let serial_of_partition = |part: &str, _configured: &[String]| {
                self.devices
                    .iter()
                    .find(|(_, dev)| part.starts_with(dev.as_str()))
                    .map(|(s, _)| s.clone())
                    .ok_or_else(|| format!("{part}: no attached drive carries it"))
            };
            f(&MountProbes {
                is_mountpoint: &is_mountpoint,
                find_mount_for_device: &find_mount_for_device,
                device_from_serial: &device_from_serial,
                device_for_uuid: &device_for_uuid,
                missing_devids: &missing_devids,
                serial_of_partition: &serial_of_partition,
            })
        }

        fn mount_targets(
            &self,
            config: &Config,
            progress: &Recorder,
            runner: &Arc<ScriptedRunner>,
        ) -> Result<MountGuard, MountError> {
            self.mount_targets_held(config, progress, runner, &MaintenanceHeld::assumed())
        }

        fn mount_targets_held(
            &self,
            config: &Config,
            progress: &Recorder,
            runner: &Arc<ScriptedRunner>,
            held: &MaintenanceHeld,
        ) -> Result<MountGuard, MountError> {
            let raid_dir = self
                .raid_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("/nonexistent/das-raid-test-record-dir"));
            self.with_clear(runner, |clear| {
                self.with_probes(runner, |p| {
                    ensure_targets_mounted_with(
                        config,
                        self.intent.unwrap_or(raid_guard::Intent::Write),
                        progress,
                        held,
                        p,
                        runner.clone(),
                        RaidSite {
                            record_dir: &raid_dir,
                            missing: {
                                let at_end = self.missing_at_end.clone();
                                std::rc::Rc::new(move |uuid: &str| {
                                    Ok(at_end
                                        .iter()
                                        .find(|(u, _)| u == uuid)
                                        .map(|(_, m)| m.clone())
                                        .unwrap_or_default())
                                })
                            },
                            clear,
                        },
                    )
                })
            })
        }

        fn mount_sources(
            &self,
            config: &Config,
            progress: &Recorder,
            runner: &Arc<ScriptedRunner>,
        ) -> MountGuard {
            self.with_probes(runner, |p| {
                ensure_sources_mounted_with(config, progress, p, runner.clone())
            })
        }

        fn verify(
            &self,
            targets: &[Target],
            write_labels: &[&str],
            progress: &Recorder,
            runner: &ScriptedRunner,
        ) -> Result<(), String> {
            let labels: Vec<String> = write_labels.iter().map(|l| l.to_string()).collect();
            self.with_probes(runner, |p| {
                verify_write_targets_with(targets, &labels, progress, p, runner)
            })
        }
    }

    /// Stages, steps and log lines kept in one place, with `assert_only_log`.
    /// (`TestProgress` records them too, but did not record steps when this was
    /// written, and these tests read steps and logs together.)
    #[derive(Default)]
    struct Recorder {
        stages: Mutex<Vec<(String, u64)>>,
        steps: Mutex<Vec<(u64, u64, String)>>,
        logs: Mutex<Vec<(LogLevel, String)>>,
    }

    impl ProgressCallback for Recorder {
        fn on_stage(&self, stage: &str, total_steps: u64) {
            self.stages
                .lock()
                .unwrap()
                .push((stage.to_string(), total_steps));
        }
        fn on_progress(&self, current: u64, total: u64, message: &str) {
            self.steps
                .lock()
                .unwrap()
                .push((current, total, message.to_string()));
        }
        fn on_throughput(&self, _: u64) {}
        fn on_log(&self, level: LogLevel, message: &str) {
            self.logs.lock().unwrap().push((level, message.to_string()));
        }
        fn on_complete(&self, _: bool, _: &str) {}
    }

    impl Recorder {
        fn stages(&self) -> Vec<(String, u64)> {
            self.stages.lock().unwrap().clone()
        }
        fn steps(&self) -> Vec<(u64, u64, String)> {
            self.steps.lock().unwrap().clone()
        }
        fn logs(&self) -> Vec<(LogLevel, String)> {
            self.logs.lock().unwrap().clone()
        }
        /// Exactly one log line, at `level`, containing every fragment.
        #[track_caller]
        fn assert_only_log(&self, level: LogLevel, fragments: &[&str]) {
            let logs = self.logs();
            assert_eq!(logs.len(), 1, "expected exactly one log line: {logs:?}");
            assert_eq!(logs[0].0, level, "{logs:?}");
            for fragment in fragments {
                assert!(
                    logs[0].1.contains(fragment),
                    "missing {fragment:?}: {logs:?}"
                );
            }
        }
    }

    /// A scratch tree standing in for `/mnt` and `/dev`: mount points are
    /// directories under it, "block devices" are plain files.
    struct Scratch {
        dir: tempfile::TempDir,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                dir: tempfile::TempDir::new().unwrap(),
            }
        }

        fn path(&self, rel: &str) -> String {
            self.dir.path().join(rel).to_string_lossy().into_owned()
        }

        fn mkdir(&self, rel: &str) -> String {
            std::fs::create_dir_all(self.dir.path().join(rel)).unwrap();
            self.path(rel)
        }

        fn touch(&self, rel: &str) -> String {
            let p = self.dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"").unwrap();
            self.path(rel)
        }
    }

    fn target(label: &str, serial: &str, mount: &str, role: TargetRole) -> Target {
        Target {
            serial: serial.into(),
            serials: vec![serial.into()],
            role,
            ..target_at(label, mount, None)
        }
    }

    fn source(label: &str, device: &str, volume: &str) -> Source {
        Source {
            label: label.into(),
            volume: volume.into(),
            subvolumes: Vec::new(),
            device: device.into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: Vec::new(),
            target_labels: Vec::new(),
            static_subvolumes: Vec::new(),
        }
    }

    fn config_with(targets: Vec<Target>, sources: Vec<Source>) -> Config {
        Config {
            targets,
            sources,
            ..Config::default()
        }
    }

    /// One primary target whose drive is attached (`<scratch>/dev/sdb`, with
    /// its first partition present) and whose mount point is `<scratch>/mnt/p`
    /// — not created, not mounted.
    fn attached_primary(scratch: &Scratch) -> (Config, Host, String, String) {
        let part = scratch.touch("dev/sdb1");
        let mnt = scratch.path("mnt/p");
        let config = config_with(
            vec![target("primary", "SER-P", &mnt, TargetRole::Primary)],
            Vec::new(),
        );
        let host = Host {
            devices: vec![("SER-P".into(), scratch.path("dev/sdb"))],
            ..Host::default()
        };
        (config, host, part, mnt)
    }

    /// bd su1b.33: `SystemMembers::remount_rw` asks for exactly
    /// `mount -o remount,rw <mount>` and reports the command's failure.
    #[test]
    fn remount_rw_runs_the_remount_and_propagates_failure() {
        use raid_guard::MemberHost;
        let probes = HOST_PROBES;
        let ok = ScriptedRunner::succeeding();
        let host = SystemMembers {
            runner: ok.as_ref(),
            probes: &probes,
        };
        assert_eq!(host.remount_rw("/mnt/t"), Ok(()));
        assert_eq!(
            argvs(&ok, "mount"),
            vec![vec!["mount", "-o", "remount,rw", "/mnt/t"]]
        );
        let bad = ScriptedRunner::with_rules(&[(
            "remount,rw",
            Reply::Exit {
                code: 32,
                stdout: "",
                stderr: "wrong fs type",
            },
        )]);
        let host = SystemMembers {
            runner: bad.as_ref(),
            probes: &probes,
        };
        let e = host.remount_rw("/mnt/t").unwrap_err();
        assert!(e.contains("mount -o remount,rw") && e.contains("32"), "{e}");
        assert!(e.contains("wrong fs type"), "{e}");
    }

    /// bd su1b.33: the opened devices are read from the kernel's sysfs for
    /// this filesystem; one the kernel does not know is an error, never a
    /// made-up list. Reads `/sys/fs/btrfs` only, a UUID no host has.
    #[test]
    fn opened_devices_of_an_unknown_filesystem_is_an_error() {
        use raid_guard::MemberHost;
        let probes = HOST_PROBES;
        let runner = ScriptedRunner::succeeding();
        let host = SystemMembers {
            runner: runner.as_ref(),
            probes: &probes,
        };
        assert!(
            host.opened_devices("00000000-0000-4000-8000-0000000033ff")
                .is_err()
        );
        assert!(runner.calls().is_empty());
    }

    /// bd su1b.33: the preview refuses a label no target carries, before
    /// touching anything.
    #[test]
    fn raid_accept_preview_of_an_unknown_label_is_an_error() {
        let config = config_with(Vec::new(), Vec::new());
        let e = raid_accept_preview(
            &config,
            "nope",
            Path::new("/nonexistent/das-33.db"),
            &Recorder::default(),
            &MaintenanceHeld::assumed(),
        )
        .unwrap_err();
        assert_eq!(e, "no [[target]] labelled 'nope'");
    }

    fn guard_holding(runner: &Arc<ScriptedRunner>, mounts: &[&str]) -> MountGuard {
        let mut guard = MountGuard::new(runner.clone());
        guard
            .newly_mounted
            .extend(mounts.iter().map(|m| m.to_string()));
        guard
    }

    // -----------------------------------------------------------------------
    // SystemRunner — the only code that spawns; exercised with harmless
    // binaries so the scripted runner above is known to stand in for
    // something that behaves the same way.
    // -----------------------------------------------------------------------

    #[test]
    fn system_runner_status_reports_the_real_exit() {
        let ok = SystemRunner.status(&mut Command::new("true")).unwrap();
        assert!(ok.success());

        let failed = SystemRunner.status(&mut Command::new("false")).unwrap();
        assert!(!failed.success());
        assert_eq!(failed.code(), Some(1));

        assert!(
            SystemRunner
                .status(&mut Command::new("/nonexistent/das-backup-test-binary"))
                .is_err()
        );
    }

    #[test]
    fn system_runner_output_captures_exit_and_both_streams() {
        let out = SystemRunner
            .output(Command::new("sh").args(["-c", "printf out; printf err >&2; exit 3"]))
            .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out");
        assert_eq!(out.stderr, b"err");

        let ok = SystemRunner
            .output(Command::new("sh").args(["-c", "exit 0"]))
            .unwrap();
        assert!(ok.status.success());

        assert!(
            SystemRunner
                .output(&mut Command::new("/nonexistent/das-backup-test-binary"))
                .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // MountGuard
    // -----------------------------------------------------------------------

    /// Explicit unmount releases everything the guard holds, last-mounted
    /// first, and reports each one — after which Drop has nothing left to do.
    #[test]
    fn unmount_releases_in_reverse_order_and_reports_each_step() {
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();
        let mut guard = guard_holding(&runner, &["/t/first", "/t/second"]);

        assert!(guard.unmount(&progress).is_empty());

        assert_eq!(
            runner.calls(),
            vec![
                status_call(&["umount", "/t/second"]),
                status_call(&["umount", "/t/first"]),
            ]
        );
        assert_eq!(progress.stages(), vec![("Unmounting targets".into(), 2)]);
        assert_eq!(
            progress.steps(),
            vec![
                (1, 2, "Unmounted /t/second".to_string()),
                (2, 2, "Unmounted /t/first".to_string()),
            ]
        );
        assert!(progress.logs().is_empty(), "{:?}", progress.logs());
        assert_eq!(guard.count(), 0);

        drop(guard);
        assert_eq!(runner.calls().len(), 2, "Drop must not unmount twice");
    }

    /// A umount that never succeeds is retried, then reported as still
    /// mounted — an error, never a progress step: reporting "Unmounted X" for
    /// a target that is still mounted is how the next run's mount-point
    /// reasoning goes wrong.
    #[test]
    fn unmount_returns_each_mount_point_it_could_not_release() {
        let runner = ScriptedRunner::with_rules(&[
            ("/t/busy", Reply::exit(32)),
            ("/t/norun", Reply::CannotRun),
        ]);
        let progress = Recorder::default();
        let mut guard = guard_holding(&runner, &["/t/busy", "/t/norun"]);

        let still = guard.unmount(&progress);

        assert_eq!(still, vec!["/t/norun".to_string(), "/t/busy".to_string()]);
        let attempts = UMOUNT_ATTEMPTS as usize;
        assert_eq!(runner.calls().len(), 2 * attempts);
        assert_eq!(
            *runner.pauses.lock().unwrap(),
            vec![UMOUNT_RETRY_PAUSE; 2 * (attempts - 1)],
            "a pause between attempts, none after the last"
        );
        assert!(progress.steps().is_empty(), "{:?}", progress.steps());
        let logs = progress.logs();
        let errors: Vec<&String> = logs
            .iter()
            .filter(|(l, _)| *l == LogLevel::Error)
            .map(|(_, m)| m)
            .collect();
        assert_eq!(errors.len(), 2, "{logs:?}");
        assert!(
            errors[0].starts_with("/t/norun is still mounted: umount failed to run:"),
            "{logs:?}"
        );
        assert_eq!(
            errors[1],
            "/t/busy is still mounted: umount exited with code 32 after 5 attempts"
        );
        assert_eq!(
            logs.iter().filter(|(l, _)| *l == LogLevel::Warning).count(),
            2 * (attempts - 1),
            "one warning per retried attempt: {logs:?}"
        );
        assert_eq!(guard.count(), 0);
        drop(guard);
        assert_eq!(
            runner.calls().len(),
            2 * attempts,
            "Drop must not retry again"
        );
    }

    /// A target busy for a moment is released on a later attempt, and the
    /// job sees nothing left mounted.
    #[test]
    fn a_target_busy_twice_is_released_on_the_third_attempt() {
        let runner = ScriptedRunner::with_rules(&[("/t/busy", Reply::Seq(&[32, 32, 0]))]);
        let progress = Recorder::default();
        let mut guard = guard_holding(&runner, &["/t/busy"]);

        let still = guard.unmount(&progress);

        assert!(still.is_empty(), "{still:?}");
        assert_eq!(
            runner.calls(),
            vec![status_call(&["umount", "/t/busy"]); 3],
            "stops trying once it succeeds"
        );
        assert_eq!(*runner.pauses.lock().unwrap(), vec![UMOUNT_RETRY_PAUSE; 2]);
        assert_eq!(
            progress.steps(),
            vec![(1, 1, "Unmounted /t/busy".to_string())]
        );
        assert_eq!(
            progress.logs(),
            vec![
                (
                    LogLevel::Warning,
                    "/t/busy: umount exited with code 32 (attempt 1 of 5) — retrying".to_string()
                ),
                (
                    LogLevel::Warning,
                    "/t/busy: umount exited with code 32 (attempt 2 of 5) — retrying".to_string()
                ),
            ]
        );
    }

    /// Busy on every attempt but the last still counts as released; busy on
    /// the last too is not.
    #[test]
    fn the_fifth_attempt_is_the_last_one() {
        let runner = ScriptedRunner::with_rules(&[("/t/late", Reply::Seq(&[32, 32, 32, 32, 0]))]);
        let mut guard = guard_holding(&runner, &["/t/late"]);
        assert!(guard.unmount(&Recorder::default()).is_empty());
        assert_eq!(runner.calls().len(), 5);

        let runner =
            ScriptedRunner::with_rules(&[("/t/never", Reply::Seq(&[32, 32, 32, 32, 32, 0]))]);
        let mut guard = guard_holding(&runner, &["/t/never"]);
        assert_eq!(
            guard.unmount(&Recorder::default()),
            vec!["/t/never".to_string()]
        );
        assert_eq!(runner.calls().len(), 5, "no sixth attempt");
    }

    #[test]
    fn a_clean_unmount_takes_no_pause() {
        let runner = ScriptedRunner::succeeding();
        let mut guard = guard_holding(&runner, &["/t/a", "/t/b"]);
        assert!(guard.unmount(&Recorder::default()).is_empty());
        assert!(runner.pauses.lock().unwrap().is_empty());
    }

    #[test]
    fn still_mounted_error_names_every_path_or_nothing() {
        assert_eq!(still_mounted_error(&[]), None);
        assert_eq!(
            still_mounted_error(&["/mnt/a".into(), "/mnt/b".into()]).as_deref(),
            Some("still mounted: /mnt/a, /mnt/b")
        );
    }

    #[test]
    fn the_real_runner_really_pauses() {
        let start = std::time::Instant::now();
        SystemRunner.pause(Duration::from_millis(60));
        assert!(start.elapsed() >= Duration::from_millis(60));
        assert!(UMOUNT_RETRY_PAUSE >= Duration::from_secs(1));
    }

    /// What a backup job sees through `backup::Release`: the same list.
    #[test]
    fn release_reports_what_unmount_could_not_release() {
        use crate::backup::Release;
        let runner = ScriptedRunner::with_rules(&[("/t/busy", Reply::exit(32))]);
        let mut guard = guard_holding(&runner, &["/t/busy", "/t/fine"]);
        assert_eq!(
            Release::release(&mut guard, &Recorder::default()),
            vec!["/t/busy".to_string()]
        );
    }

    #[test]
    fn unmount_on_an_empty_guard_does_nothing() {
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();
        let mut guard = MountGuard::new(runner.clone());

        assert!(guard.unmount(&progress).is_empty());
        drop(guard);

        assert!(runner.calls().is_empty());
        assert!(progress.stages().is_empty());
    }

    /// Drop is the sole cleanup when a job panics or is aborted, so it must
    /// issue the unmounts itself (bd DAS-Backup-Manager-06p).
    #[test]
    fn drop_unmounts_whatever_is_still_held_in_reverse_order() {
        let runner = ScriptedRunner::succeeding();

        drop(guard_holding(&runner, &["/t/first", "/t/second"]));

        assert_eq!(
            runner.calls(),
            vec![
                status_call(&["umount", "/t/second"]),
                status_call(&["umount", "/t/first"]),
            ]
        );
    }

    /// What Drop prints: nothing for a clean unmount, one line for each mount
    /// point left behind after every attempt.
    #[test]
    fn unmount_remaining_reports_only_the_failures() {
        let runner = ScriptedRunner::with_rules(&[
            ("/t/busy", Reply::exit(32)),
            ("/t/norun", Reply::CannotRun),
            ("/t/flaky", Reply::Seq(&[32, 0])),
        ]);
        let mut guard = guard_holding(&runner, &["/t/busy", "/t/fine", "/t/flaky", "/t/norun"]);

        let failures = guard.unmount_remaining();

        let attempts = UMOUNT_ATTEMPTS as usize;
        assert_eq!(runner.calls().len(), 2 * attempts + 1 + 2);
        assert_eq!(runner.pauses.lock().unwrap().len(), 2 * (attempts - 1) + 1);
        assert_eq!(guard.count(), 0);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(
            failures[0]
                .starts_with("MountGuard::drop: /t/norun is still mounted: umount failed to run:"),
            "{failures:?}"
        );
        assert_eq!(
            failures[1],
            "MountGuard::drop: /t/busy is still mounted: umount exited with code 32 after 5 attempts"
        );

        let clean = ScriptedRunner::succeeding();
        assert!(
            guard_holding(&clean, &["/t/fine"])
                .unmount_remaining()
                .is_empty()
        );
        assert_eq!(clean.calls(), vec![status_call(&["umount", "/t/fine"])]);
    }

    // -----------------------------------------------------------------------
    // ensure_targets_mounted
    // -----------------------------------------------------------------------

    /// A target an earlier job left mounted (bd DAS-Backup-Manager-jgm,
    /// operator 2026-11-09): once it is shown to be the target, it is
    /// unmounted and mounted afresh, so this run owns — and gives back — the
    /// mount it uses.
    #[test]
    fn a_leftover_target_is_unmounted_and_mounted_afresh_for_this_run() {
        let scratch = Scratch::new();
        let (mut config, mut host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        let runner = ScriptedRunner::with_rules(&[(
            "UUID",
            Reply::Exit {
                code: 0,
                stdout: "u-p\n",
                stderr: "",
            },
        )]);
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(
            sans_blkid(&runner),
            vec![
                output_call(&["findmnt", "-n", "-o", "UUID", "--target", &mnt]),
                status_call(&["umount", &mnt]),
                output_call(&["mount", "-t", "btrfs", "UUID=u-p", &mnt]),
            ]
        );
        assert_eq!(guard.count(), 1, "the fresh mount is this run's");
        assert_eq!(
            progress.logs(),
            vec![(
                LogLevel::Warning,
                format!(
                    "Target 'primary': {mnt} was left mounted by an earlier job — unmounted, \
                     to be mounted afresh for this run"
                )
            )]
        );
        drop(guard);
        assert_eq!(sans_blkid(&runner)[3..], [status_call(&["umount", &mnt])]);
    }

    /// The plain unmount is retried; when every try finds it busy, one
    /// `umount -f` follows before anything is refused.
    #[test]
    fn a_leftover_the_plain_unmount_cannot_release_is_forced() {
        let scratch = Scratch::new();
        let (mut config, mut host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        let runner = ScriptedRunner::with_rules(&[
            (
                "UUID",
                Reply::Exit {
                    code: 0,
                    stdout: "u-p\n",
                    stderr: "",
                },
            ),
            ("umount", Reply::Seq(&[32, 32, 32, 32, 32, 0])),
        ]);
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        let calls = sans_blkid(&runner);
        assert_eq!(calls[1..6], vec![status_call(&["umount", &mnt]); 5][..]);
        // Still busy: whether btrfs maintenance runs on it is asked first
        // (bd su1b.12); nothing holds it, so straight to the forced unmount.
        assert_eq!(calls[6], output_call(&["btrfs", "scrub", "status", &mnt]));
        assert_eq!(calls[7], status_call(&["umount", "-f", &mnt]));
        assert_eq!(
            calls[8],
            output_call(&["mount", "-t", "btrfs", "UUID=u-p", &mnt])
        );
        assert_eq!(guard.count(), 1);
        assert!(
            progress
                .logs()
                .iter()
                .any(|(level, msg)| *level == LogLevel::Warning
                    && msg
                        == &format!(
                            "{mnt}: umount exited with code 32 after {UMOUNT_ATTEMPTS} attempts — \
                     retrying with umount -f"
                        )),
            "{:?}",
            progress.logs()
        );
    }

    /// Only when the forced unmount fails too is the job refused — before it
    /// mounts anything, and with the target still mounted as it was.
    #[test]
    fn a_leftover_that_survives_the_forced_unmount_refuses_the_job() {
        let scratch = Scratch::new();
        let (mut config, mut host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        let runner = ScriptedRunner::with_rules(&[
            (
                "UUID",
                Reply::Exit {
                    code: 0,
                    stdout: "u-p\n",
                    stderr: "",
                },
            ),
            ("umount", Reply::Seq(&[32])),
        ]);

        let err = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .err()
            .expect("a leftover that cannot be unmounted refuses the job");

        assert_eq!(
            err.to_string(),
            format!(
                "Target 'primary': {mnt} was left mounted by an earlier job and cannot be \
                 unmounted (umount exited with code 32 after {UMOUNT_ATTEMPTS} attempts; \
                 umount -f exited with code 32) — refusing to run until it is"
            )
        );
        assert!(
            !runner.calls().iter().any(|c| c.argv[0] == "mount"),
            "{:?}",
            runner.calls()
        );
    }

    // --- bd DAS-Backup-Manager-su1b.12: another program's mount, and its
    // holders ------------------------------------------------------------

    /// A RAID-1 target, both drives attached, whose filesystem udisks also
    /// mounted under /run/media (mount id 77), with `holder` using it.
    fn raid1_mounted_by_udisks(
        scratch: &Scratch,
        holder: crate::holders::testproc::Proc<'_>,
    ) -> (Config, Host, Arc<ScriptedRunner>, String) {
        let (mut config, mut host, part_b, mnt) = attached_primary(scratch);
        let part_c = scratch.touch("dev/sdc1");
        config.targets[0].mount_uuid = Some("u-p".into());
        config.targets[0].serials = vec!["SER-P".into(), "SER-Q".into()];
        config.das.mount_opts = "noatime,compress=zstd".into();
        config.das.member_wait_secs = 0;
        host.devices.push(("SER-Q".into(), scratch.path("dev/sdc")));
        host.mounted_elsewhere = vec![("SER-P".into(), "/run/media/u/das".into())];
        host.raid_dir = Some(PathBuf::from(scratch.path("raid")));
        host.mountinfo = vec![
            "22 1 0:21 / / rw - btrfs /dev/root0 rw".into(),
            format!(
                "77 22 0:52 / /run/media/u/das rw,nosuid - btrfs {part_b} rw,subvolid=5,subvol=/"
            ),
        ];
        let proc = crate::holders::testproc::FakeProc::new();
        proc.add(holder);
        host.proc = Some(Arc::new(proc));
        let runner = ScriptedRunner::with_rules(&[
            ("blkid", Reply::Out(0, format!("{part_b}\n{part_c}\n"))),
            (
                &part_b,
                Reply::Out(
                    0,
                    super_of("u-p", 1, "5eed00a1-0000-4000-8000-0000000000a1", 40),
                ),
            ),
            (
                &part_c,
                Reply::Out(
                    0,
                    super_of("u-p", 2, "5eed00a2-0000-4000-8000-0000000000a2", 40),
                ),
            ),
        ]);
        (config, host, runner, mnt)
    }

    // --- bd DAS-Backup-Manager-eg4j.20: the scrub engine's guarded mount --

    /// The scrub engine's guarded mount of the RAID-1 target on the host
    /// `raid1_mounted_by_udisks` describes, under `held`.
    fn scrub_guarded_mount(
        config: &Config,
        host: &Host,
        runner: &Arc<ScriptedRunner>,
        progress: &Recorder,
        delegated: bool,
    ) -> Result<raid_guard::Outcome, raid_guard::Failure> {
        std::fs::create_dir_all(&config.targets[0].mount).unwrap();
        let raid_dir = host.raid_dir.clone().unwrap();
        host.with_clear(runner, |clear| {
            host.with_probes(runner, |p| {
                mount_guarded_clearing_with(
                    config,
                    &config.targets[0],
                    raid_guard::Job::Scrub.intent(),
                    &raid_dir,
                    progress,
                    p,
                    &**runner,
                    clear,
                    delegated,
                )
            })
        })
    }

    /// A scrub of a RAID-1 target another program mounted elsewhere clears
    /// that mount first, as a writing job, then mounts through the guard.
    #[test]
    fn scrub_guarded_mount_clears_another_programs_idle_mount_first() {
        let scratch = Scratch::new();
        let (config, host, runner, mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        let progress = Recorder::default();

        let got = scrub_guarded_mount(&config, &host, &runner, &progress, false);

        assert!(got.is_ok(), "{:?}", got.err());
        assert!(!binds(&runner), "{:?}", runner.calls());
        assert_eq!(host.signals(), vec![(300, crate::holders::Sig::Term)]);
        let calls = runner.calls();
        let umount = calls
            .iter()
            .position(|c| c == &status_call(&["umount", "--no-canonicalize", "/run/media/u/das"]))
            .expect("the other program's mount is unmounted");
        let mount = calls
            .iter()
            .position(|c| {
                c == &output_call(&[
                    "mount",
                    "-t",
                    "btrfs",
                    "-o",
                    "noatime,compress=zstd",
                    "UUID=u-p",
                    &mnt,
                ])
            })
            .expect("the guarded mount at the target's own path");
        assert!(umount < mount, "{calls:?}");
    }

    /// Another program's writer is never stopped: the target is refused, the
    /// writer named, and nothing is mounted or signalled.
    #[test]
    fn scrub_guarded_mount_refused_for_another_programs_writer() {
        let scratch = Scratch::new();
        let writer = crate::holders::testproc::Proc {
            pid: 301,
            cmd: "rsync",
            fds: vec![(4, "/run/media/u/das/big", "0100001", Some(77))],
            ..Default::default()
        };
        let (config, host, runner, _mnt) = raid1_mounted_by_udisks(&scratch, writer);

        let got = scrub_guarded_mount(&config, &host, &runner, &Recorder::default(), false);

        let Err(raid_guard::Failure::Refused { why, left_mounted }) = got else {
            panic!("{got:?}");
        };
        assert!(why.contains("pid 301"), "{why}");
        assert!(!left_mounted);
        assert!(host.signals().is_empty());
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv[0] == "mount" || c.argv[0] == "umount"),
            "{:?}",
            runner.calls()
        );
    }

    /// Under a handed-down hold nothing is cleared and nothing is mounted
    /// beside the other program's mount: not scrubbed, the mount named.
    #[test]
    fn scrub_guarded_mount_under_a_delegated_hold_clears_and_mounts_nothing() {
        let scratch = Scratch::new();
        let (config, host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));

        let got = scrub_guarded_mount(&config, &host, &runner, &Recorder::default(), true);

        let Err(raid_guard::Failure::Refused { why, .. }) = got else {
            panic!("{got:?}");
        };
        assert!(why.contains("/run/media/u/das"), "{why}");
        assert!(host.signals().is_empty());
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv[0] == "mount" || c.argv[0] == "umount"),
            "{:?}",
            runner.calls()
        );
    }

    // --- bd DAS-Backup-Manager-eg4j.19: single-drive targets --------------

    /// A single-drive target (one serial, a `mount_uuid`) whose filesystem
    /// udisks also mounted under /run/media (mount id 77), with `holder`
    /// using it. `uuid`: whether the target is configured with `mount_uuid`.
    fn single_mounted_by_udisks(
        scratch: &Scratch,
        holder: Option<crate::holders::testproc::Proc<'_>>,
        uuid: bool,
    ) -> (Config, Host, Arc<ScriptedRunner>, String) {
        let (mut config, mut host, part_b, mnt) = attached_primary(scratch);
        config.targets[0].mount_uuid = uuid.then(|| "u-p".to_string());
        config.das.mount_opts = "noatime".into();
        host.mounted_elsewhere = vec![("SER-P".into(), "/run/media/u/das".into())];
        host.uuids = vec![("u-p".into(), part_b.clone())];
        host.mountinfo = vec![
            "22 1 0:21 / / rw - btrfs /dev/root0 rw".into(),
            format!(
                "77 22 0:52 / /run/media/u/das rw,nosuid - btrfs {part_b} rw,subvolid=5,subvol=/"
            ),
        ];
        let proc = crate::holders::testproc::FakeProc::new();
        if let Some(h) = holder {
            proc.add(h);
        }
        host.proc = Some(Arc::new(proc));
        let runner = ScriptedRunner::with_rules(&[("blkid", Reply::Out(0, format!("{part_b}\n")))]);
        (config, host, runner, mnt)
    }

    fn single_mount_call(mnt: &str) -> Call {
        output_call(&["mount", "-t", "btrfs", "-o", "noatime", "UUID=u-p", mnt])
    }

    #[test]
    fn single_drive_target_mounted_elsewhere_is_cleared_then_mounted() {
        let scratch = Scratch::new();
        let (config, host, runner, mnt) =
            single_mounted_by_udisks(&scratch, Some(reader_at(300, "/run/media/u/das/a")), true);
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert!(!binds(&runner), "bound as found: {:?}", runner.calls());
        assert_eq!(host.signals(), vec![(300, crate::holders::Sig::Term)]);
        let calls = runner.calls();
        let umount = calls
            .iter()
            .position(|c| c == &status_call(&["umount", "--no-canonicalize", "/run/media/u/das"]))
            .expect("the other program's mount is unmounted");
        let mount = calls
            .iter()
            .position(|c| c == &single_mount_call(&mnt))
            .expect("the target mounted at its own path");
        assert!(umount < mount, "{calls:?}");
        assert_eq!(guard.count(), 1);
        assert!(logged(&progress, "pid 300"), "{:?}", progress.logs());
        assert_eq!(guard.clear_notes()[0].outcome, ClearOutcome::Stopped);
    }

    #[test]
    fn single_drive_target_writer_elsewhere_not_available() {
        let scratch = Scratch::new();
        let writer = crate::holders::testproc::Proc {
            pid: 301,
            cmd: "rsync",
            fds: vec![(4, "/run/media/u/das/big", "0100001", Some(77))],
            ..Default::default()
        };
        let (config, host, runner, mnt) = single_mounted_by_udisks(&scratch, Some(writer), true);
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        let Err(MountError::AllTargetsRefused { notes }) = result else {
            panic!("{:?}", result.err());
        };
        assert_eq!(notes[0].outcome, ClearOutcome::Refused);
        assert!(notes[0].detail.contains("pid 301"), "{notes:?}");
        assert!(host.signals().is_empty());
        assert!(!binds(&runner));
        assert!(!runner.calls().contains(&single_mount_call(&mnt)));
        assert!(logged(&progress, "NOT available"), "{:?}", progress.logs());
    }

    /// An idle mount of a single-drive target with a `mount_uuid` is never
    /// bound (a backup written through another program's mount): unmounted,
    /// then the target mounted as configured.
    #[test]
    fn single_drive_target_never_bind_mounted_when_uuid_configured() {
        let scratch = Scratch::new();
        let (config, host, runner, mnt) = single_mounted_by_udisks(&scratch, None, true);

        let guard = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .unwrap();

        assert!(!binds(&runner), "{:?}", runner.calls());
        assert!(runner.calls().contains(&single_mount_call(&mnt)));
        assert_eq!(guard.count(), 1);
    }

    /// No `mount_uuid`: nothing identifies the filesystem to look for, so the
    /// bind fallback stays for it (bd 804's WARN).
    #[test]
    fn target_without_uuid_still_bind_mounts() {
        let scratch = Scratch::new();
        let (config, host, runner, _mnt) = single_mounted_by_udisks(&scratch, None, false);

        host.mount_targets(&config, &Recorder::default(), &runner)
            .unwrap();

        assert!(binds(&runner), "{:?}", runner.calls());
        // Only the guard's own take-back of what it bound, never the other
        // program's mount.
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv.iter().any(|a| a == "/run/media/u/das") && c.argv[0] == "umount"),
            "{:?}",
            runner.calls()
        );
    }

    /// Under a handed-down hold nothing is cleared or bound: NOT available
    /// (the parent owns the mounts).
    #[test]
    fn delegated_hold_single_drive_elsewhere_not_available() {
        let scratch = Scratch::new();
        let (config, host, runner, _mnt) =
            single_mounted_by_udisks(&scratch, Some(reader_at(300, "/run/media/u/das/a")), true);
        let progress = Recorder::default();

        let result = host.mount_targets_held(
            &config,
            &progress,
            &runner,
            &MaintenanceHeld::delegated_for_test(),
        );

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(!binds(&runner));
        assert!(host.signals().is_empty());
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv[0] == "mount" || c.argv[0] == "umount"),
            "{:?}",
            runner.calls()
        );
    }

    /// A job that writes nothing under the target (the doctor, walk, restore,
    /// the recovery-OS reading) never stops another program's reader of a
    /// single-drive target: refused, the reader named, nothing signalled.
    #[test]
    fn doctor_single_drive_reader_elsewhere_refused_nothing_signalled() {
        let scratch = Scratch::new();
        let (config, mut host, runner, _mnt) =
            single_mounted_by_udisks(&scratch, Some(reader_at(300, "/run/media/u/das/a")), true);
        host.read_only_job = true;
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(host.signals().is_empty());
        assert!(!runner.calls().iter().any(|c| c.argv[0] == "umount"));
        assert!(
            logged(&progress, "never stops a process") && logged(&progress, "pid 300"),
            "{:?}",
            progress.logs()
        );
    }

    /// The scrub engine's mount of a single-drive target clears another
    /// program's mount first, as a job that writes (a scrub repairs).
    #[test]
    fn scrub_single_drive_mounted_elsewhere_cleared_then_scrubbed() {
        let scratch = Scratch::new();
        let (_config, host, runner, mnt) =
            single_mounted_by_udisks(&scratch, Some(reader_at(300, "/run/media/u/das/a")), true);
        let progress = Recorder::default();

        let got = host.with_clear(&runner, |c| {
            host.with_probes(&runner, |p| {
                clear_elsewhere_with(&*runner, p, c, "primary", "u-p", &mnt, false, &progress)
            })
        });

        let sent = got.unwrap().expect("cleared");
        assert!(sent[0].starts_with("SIGTERM to pid 300"), "{sent:?}");
        assert_eq!(host.signals(), vec![(300, crate::holders::Sig::Term)]);
        assert!(runner.calls().contains(&status_call(&[
            "umount",
            "--no-canonicalize",
            "/run/media/u/das"
        ])));
    }

    #[test]
    fn scrub_single_drive_writer_elsewhere_not_scrubbed() {
        let scratch = Scratch::new();
        let writer = crate::holders::testproc::Proc {
            pid: 301,
            cmd: "rsync",
            fds: vec![(4, "/run/media/u/das/big", "0100001", Some(77))],
            ..Default::default()
        };
        let (_config, host, runner, mnt) = single_mounted_by_udisks(&scratch, Some(writer), true);

        let got = host.with_clear(&runner, |c| {
            host.with_probes(&runner, |p| {
                clear_elsewhere_with(
                    &*runner,
                    p,
                    c,
                    "primary",
                    "u-p",
                    &mnt,
                    false,
                    &Recorder::default(),
                )
            })
        });

        assert!(got.unwrap_err().contains("pid 301"));
        assert!(host.signals().is_empty());
        assert!(!runner.calls().iter().any(|c| c.argv[0] == "umount"));
    }

    /// Under a handed-down hold the scrub's mount does not clear either.
    #[test]
    fn scrub_under_a_delegated_hold_does_not_clear_another_programs_mount() {
        let scratch = Scratch::new();
        let (_config, host, runner, mnt) =
            single_mounted_by_udisks(&scratch, Some(reader_at(300, "/run/media/u/das/a")), true);

        let got = host.with_clear(&runner, |c| {
            host.with_probes(&runner, |p| {
                clear_elsewhere_with(
                    &*runner,
                    p,
                    c,
                    "primary",
                    "u-p",
                    &mnt,
                    true,
                    &Recorder::default(),
                )
            })
        });

        assert!(got.unwrap_err().contains("not cleared under a handed-down"));
        assert!(host.signals().is_empty());
        assert!(!runner.calls().iter().any(|c| c.argv[0] == "umount"));
    }

    fn reader_at(pid: i32, path: &'static str) -> crate::holders::testproc::Proc<'static> {
        crate::holders::testproc::Proc {
            pid,
            cmd: "dolphin",
            fds: vec![(3, path, "0100000", Some(77))],
            ..Default::default()
        }
    }

    fn binds(runner: &ScriptedRunner) -> bool {
        runner
            .calls()
            .iter()
            .any(|c| c.argv[0] == "mount" && c.argv.iter().any(|a| a == "--bind"))
    }

    /// The runner's commands without the `blkid` that looks for the
    /// filesystem's members before another program's mounts are cleared
    /// (bd eg4j.19): what these tests are about is what mounts.
    fn sans_blkid(runner: &ScriptedRunner) -> Vec<Call> {
        runner
            .calls()
            .into_iter()
            .filter(|c| c.argv[0] != "blkid")
            .collect()
    }

    fn logged(progress: &Recorder, fragment: &str) -> bool {
        progress.logs().iter().any(|(_, m)| m.contains(fragment))
    }

    // --- bd DAS-Backup-Manager-eg4j.19: partition-keyed clearing ----------

    const ESP_AT: &str = "/run/media/u/RECOV-ESP-1";

    /// A recovery drive's partition `/dev/sdx1` (a vfat ESP, mount id 50)
    /// mounted by udisks, with `holder` (if any) using it.
    fn esp_mounted_by_udisks(holder: Option<crate::holders::testproc::Proc<'_>>) -> Host {
        let proc = crate::holders::testproc::FakeProc::new();
        if let Some(h) = holder {
            proc.add(h);
        }
        Host {
            mountinfo: vec![
                "22 1 0:21 / / rw - btrfs /dev/root0 rw".into(),
                format!("50 22 8:17 / {ESP_AT} rw,nosuid - vfat /dev/sdx1 rw,fmask=0022"),
            ],
            block_devs: vec![
                ("/dev/sdx".into(), "8:16".into()),
                ("/dev/sdx1".into(), "8:17".into()),
                ("/dev/sdx2".into(), "8:18".into()),
            ],
            proc: Some(Arc::new(proc)),
            ..Host::default()
        }
    }

    fn esp_user(pid: i32, flags: &'static str) -> crate::holders::testproc::Proc<'static> {
        crate::holders::testproc::Proc {
            pid,
            cmd: "kate",
            fds: vec![(3, "/run/media/u/RECOV-ESP-1/x", flags, Some(50))],
            ..Default::default()
        }
    }

    fn clear_parts(
        host: &Host,
        runner: &Arc<ScriptedRunner>,
        fsid: Option<&str>,
        progress: &Recorder,
    ) -> Result<Option<Vec<String>>, String> {
        host.with_clear(runner, |c| {
            clear_partitions_with(
                &**runner,
                c,
                "recovery",
                &[crate::holders::PartitionId {
                    path: "/dev/sdx1".into(),
                    dev: "8:17".into(),
                }],
                fsid,
                progress,
            )
        })
    }

    fn umounts(runner: &ScriptedRunner) -> Vec<String> {
        runner
            .calls()
            .iter()
            .filter(|c| c.argv[0] == "umount")
            .map(|c| {
                assert_eq!(c.argv[1], "--no-canonicalize", "{c:?}");
                c.argv[2].clone()
            })
            .collect()
    }

    #[test]
    fn clear_partitions_idle_vfat_mount_is_unmounted() {
        let host = esp_mounted_by_udisks(None);
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let got = clear_parts(&host, &runner, None, &progress);

        assert_eq!(got, Ok(Some(Vec::new())));
        assert_eq!(umounts(&runner), vec![ESP_AT.to_string()]);
        assert!(host.signals().is_empty());
        let logs: Vec<String> = progress.logs().into_iter().map(|(_, m)| m).collect();
        let who = "Target 'recovery': ";
        let rows = [
            format!(
                "{who}mount    /dev/sdx1  vfat rw  {ESP_AT}  by udisks (a desktop session's \
                 automount)  -> another program's mount, not this job's"
            ),
            format!("{who}options  mount rw,nosuid  filesystem rw,fmask=0022  root /"),
            format!("{who}action   unmounted {ESP_AT} (another program's mount)"),
        ];
        assert_eq!(logs, rows, "bd 3gzs.14: one aligned row each");
    }

    /// bd 3gzs.14 security: mountinfo escapes a newline as `\012`, and
    /// decoding gives it back. A mount point holding one must not make a line
    /// of its own in what the job logs.
    #[test]
    fn a_mount_point_with_a_newline_makes_no_line_of_its_own() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo[1] =
            "50 22 8:17 / /run/media/u/x\\012CLEAR\\040TARGET:\\040OK rw,nosuid - vfat \
                             /dev/sdx1 rw"
                .into();
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let _ = clear_parts(&host, &runner, None, &progress);

        let logs = progress.logs();
        assert!(
            logs.iter()
                .any(|(_, m)| m.contains("CLEAR?TARGET") || m.contains("x?CLEAR")),
            "{logs:?}"
        );
        for (_, m) in &logs {
            assert!(!m.chars().any(char::is_control), "{m:?}");
        }
    }

    /// The status line is one line, whatever the detail carries.
    #[test]
    fn a_status_line_is_one_line_whatever_its_detail_carries() {
        let evil = "a\nCLEAR TARGET: OK  (forged)\r";
        for (line, _) in [
            ClearTarget::Refused(evil.into()).line(),
            ClearTarget::Cleared(evil.into()).line(),
            ClearTarget::CannotRun(evil.into()).line(),
        ] {
            assert!(!line.chars().any(char::is_control), "{line:?}");
            assert!(line.starts_with("CLEAR TARGET: "), "{line:?}");
        }
        for (line, _) in [
            ClearDrive::Refused(evil.into()).line(),
            ClearDrive::Cleared(evil.into()).line(),
            ClearDrive::CannotRun(evil.into()).line(),
        ] {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
        assert_eq!(
            ClearTarget::Refused("it is held (pid 1)".into()).line().0,
            "CLEAR TARGET: REFUSED  (it is held (pid 1))",
            "an ordinary detail is byte for byte"
        );
    }

    #[test]
    fn clear_partitions_mounted_nowhere_is_none_and_touches_nothing() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo.truncate(1);
        let runner = ScriptedRunner::succeeding();

        assert_eq!(
            clear_parts(&host, &runner, None, &Recorder::default()),
            Ok(None)
        );
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    #[test]
    fn clear_partitions_reader_is_stopped_when_job_writes() {
        let host = esp_mounted_by_udisks(Some(esp_user(300, "0100000")));
        let runner = ScriptedRunner::succeeding();

        let got = clear_parts(&host, &runner, None, &Recorder::default()).unwrap();

        assert_eq!(host.signals(), vec![(300, crate::holders::Sig::Term)]);
        let sent = got.expect("cleared");
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].starts_with("SIGTERM to pid 300 (kate"), "{sent:?}");
        assert_eq!(umounts(&runner), vec![ESP_AT.to_string()]);
    }

    #[test]
    fn clear_partitions_reader_refuses_read_only_job() {
        let mut host = esp_mounted_by_udisks(Some(esp_user(300, "0100000")));
        host.read_only_job = true;
        let runner = ScriptedRunner::succeeding();

        let err = clear_parts(&host, &runner, None, &Recorder::default()).unwrap_err();

        assert!(
            err.contains("never stops a process") && err.contains("pid 300"),
            "{err}"
        );
        assert!(host.signals().is_empty());
        assert!(umounts(&runner).is_empty());
    }

    #[test]
    fn clear_partitions_writer_refuses_and_signals_nothing() {
        let host = esp_mounted_by_udisks(Some(esp_user(301, "0100002")));
        let runner = ScriptedRunner::succeeding();

        let err = clear_parts(&host, &runner, None, &Recorder::default()).unwrap_err();

        assert!(err.contains("pid 301") && err.contains("writing"), "{err}");
        assert!(host.signals().is_empty());
        assert!(umounts(&runner).is_empty());
    }

    /// Review 1: a vfat ESP mounted at two places comes down deepest first.
    #[test]
    fn clear_partitions_two_mounts_come_down_deepest_first() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo
            .push("51 22 8:17 / /mnt/e rw - vfat /dev/sdx1 rw".into());
        let runner = ScriptedRunner::succeeding();

        let got = clear_parts(&host, &runner, None, &Recorder::default());

        assert_eq!(got, Ok(Some(Vec::new())));
        assert_eq!(
            umounts(&runner),
            vec![ESP_AT.to_string(), "/mnt/e".to_string()]
        );
    }

    /// Review 2: another filesystem mounted under the partition's mount is
    /// not this job's to take down.
    #[test]
    fn clear_partitions_stacked_mount_refuses() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo.push(format!(
            "60 50 0:70 / {ESP_AT}/inner rw - ext4 /dev/sdz1 rw"
        ));
        let runner = ScriptedRunner::succeeding();

        let err = clear_parts(&host, &runner, None, &Recorder::default()).unwrap_err();

        assert!(err.contains("mounted inside it"), "{err}");
        assert!(umounts(&runner).is_empty());
        assert!(host.signals().is_empty());
    }

    #[test]
    fn clear_partitions_report_only_unmounts_nothing() {
        let mut host = esp_mounted_by_udisks(Some(esp_user(300, "0100000")));
        host.report_only = true;
        let runner = ScriptedRunner::succeeding();

        let would = clear_parts(&host, &runner, None, &Recorder::default())
            .unwrap()
            .expect("would clear");

        assert!(
            would
                .iter()
                .any(|w| w.starts_with(&format!("would unmount {ESP_AT}"))),
            "{would:?}"
        );
        assert!(would.iter().any(|w| w.contains("pid 300")), "{would:?}");
        assert!(umounts(&runner).is_empty());
        assert!(host.signals().is_empty());
    }

    /// An ESP-only mount never asks btrfs anything, even when the target has
    /// a btrfs filesystem UUID.
    #[test]
    fn clear_partitions_vfat_mount_with_a_fsid_never_asks_btrfs() {
        let host = esp_mounted_by_udisks(None);
        let runner = ScriptedRunner::succeeding();

        let got = clear_parts(&host, &runner, Some("u-r"), &Recorder::default());

        assert_eq!(got, Ok(Some(Vec::new())));
        assert!(
            !runner.calls().iter().any(|c| c.argv[0] == "btrfs"),
            "{:?}",
            runner.calls()
        );
    }

    /// A btrfs mount of the partition with maintenance running is refused,
    /// as `clear_mounted_elsewhere` does.
    #[test]
    fn clear_partitions_btrfs_balance_running_refuses() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo[1] = "50 22 0:60 / /run/media/u/b rw - btrfs /dev/sdx1 rw,subvolid=5".into();
        host.exclusive = vec![("u-r".into(), "balance\n".into())];
        host.in_sysfs = vec!["u-r".into()];
        let runner = ScriptedRunner::succeeding();

        let err = clear_parts(&host, &runner, Some("u-r"), &Recorder::default()).unwrap_err();

        assert!(err.contains("balance"), "{err}");
        assert!(umounts(&runner).is_empty());
    }

    /// Review N4 (bd eg4j.19): a target with no `mount_uuid` still gets the
    /// btrfs-maintenance check on a btrfs mount of its drive, by the UUID
    /// the mount itself carries.
    #[test]
    fn clear_partitions_btrfs_balance_refuses_without_a_mount_uuid() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo[1] = "50 22 0:60 / /run/media/u/b rw - btrfs /dev/sdx1 rw,subvolid=5".into();
        host.exclusive = vec![("u-r".into(), "balance\n".into())];
        host.in_sysfs = vec!["u-r".into()];
        let runner = ScriptedRunner::with_rules(&[(
            "UUID",
            Reply::Exit {
                code: 0,
                stdout: "u-r\n",
                stderr: "",
            },
        )]);

        let err = clear_parts(&host, &runner, None, &Recorder::default()).unwrap_err();

        assert!(err.contains("balance"), "{err}");
        assert!(umounts(&runner).is_empty());
    }

    /// N4: the UUID of such a mount cannot be read → refused, never taken
    /// as "no maintenance".
    #[test]
    fn clear_partitions_btrfs_mount_whose_uuid_cannot_be_read_refuses() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo[1] = "50 22 0:60 / /run/media/u/b rw - btrfs /dev/sdx1 rw,subvolid=5".into();
        let runner = ScriptedRunner::with_rules(&[("UUID", Reply::exit(1))]);

        let err = clear_parts(&host, &runner, None, &Recorder::default()).unwrap_err();

        assert!(err.contains("filesystem UUID"), "{err}");
        assert!(umounts(&runner).is_empty());
    }

    /// The kernel has the target's btrfs mounted where this host cannot see:
    /// the answer is a refusal, not "mounted nowhere".
    #[test]
    fn clear_partitions_btrfs_mounted_only_out_of_sight_refuses() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo.truncate(1);
        host.in_sysfs = vec!["u-r".into()];
        let runner = ScriptedRunner::succeeding();

        let err = clear_parts(&host, &runner, Some("u-r"), &Recorder::default()).unwrap_err();

        assert!(err.contains("another mount namespace"), "{err}");
    }

    /// Security: another program mounts over the path between the scan and
    /// the unmount (same path, new mount id): never unmounted by path.
    #[test]
    fn clear_partitions_mount_id_changed_before_unmount_refuses_or_rescans() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo_later = Some(vec![
            "22 1 0:21 / / rw - btrfs /dev/root0 rw".into(),
            format!("99 22 0:99 / {ESP_AT} rw - tmpfs tmpfs rw"),
        ]);
        let runner = ScriptedRunner::succeeding();

        let err = clear_parts(&host, &runner, None, &Recorder::default()).unwrap_err();

        assert!(
            err.contains("no longer the mount that was scanned"),
            "{err}"
        );
        assert!(umounts(&runner).is_empty(), "{:?}", runner.calls());
    }

    /// Review N1 (bd eg4j.19): another filesystem mounted OVER the scanned
    /// mount at the same path between the scan and the unmount: the scanned
    /// id is still in the table, but not on top, and `umount <path>` would
    /// take the top one down. Refused.
    #[test]
    fn clear_partitions_overmounted_before_unmount_refuses() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo_later = Some(vec![
            "22 1 0:21 / / rw - btrfs /dev/root0 rw".into(),
            format!("50 22 8:17 / {ESP_AT} rw,nosuid - vfat /dev/sdx1 rw,fmask=0022"),
            format!("99 50 0:99 / {ESP_AT} rw - tmpfs tmpfs rw"),
        ]);
        let runner = ScriptedRunner::succeeding();

        let err = clear_parts(&host, &runner, None, &Recorder::default()).unwrap_err();

        assert!(
            err.contains("no longer the mount that was scanned"),
            "{err}"
        );
        assert!(umounts(&runner).is_empty(), "{:?}", runner.calls());
    }

    /// The path went away on its own before the unmount: nothing to unmount.
    #[test]
    fn clear_partitions_mount_gone_before_unmount_is_not_unmounted_again() {
        let mut host = esp_mounted_by_udisks(None);
        host.mountinfo_later = Some(vec!["22 1 0:21 / / rw - btrfs /dev/root0 rw".into()]);
        let runner = ScriptedRunner::succeeding();

        let got = clear_parts(&host, &runner, None, &Recorder::default());

        // The table read after it is the "later" one: nothing mounted.
        assert_eq!(got, Ok(Some(Vec::new())));
        assert!(umounts(&runner).is_empty());
    }

    /// A user's FUSE mount that calls its source `/dev/sdx1` is not the
    /// drive's: nothing unmounted, nothing signalled.
    #[test]
    fn clear_partitions_ignores_a_forged_fuse_mount_and_its_user() {
        let mut host = esp_mounted_by_udisks(Some(esp_user(300, "0100002")));
        host.mountinfo[1] =
            "50 22 0:80 / /home/u/forged rw - fuse.sshfs /dev/sdx1 rw,user_id=1000".into();
        let runner = ScriptedRunner::succeeding();

        let got = clear_parts(&host, &runner, None, &Recorder::default());

        assert_eq!(got, Ok(None));
        assert!(runner.calls().is_empty());
        assert!(host.signals().is_empty());
    }

    // --- recovery-os clear-drive -----------------------------------------

    const LSBLK_OK: &str = "/dev/sdx disk\n/dev/sdx1 part RECOV-ESP-1\n/dev/sdx2 part\n";

    /// A single-drive recovery target whose drive (serial `SER-R`) is the
    /// whole disk `/dev/sdx`.
    fn recovery_target() -> Target {
        target("recovery", "SER-R", "/mnt/r", TargetRole::Mirror)
    }

    fn cd_host(mut host: Host) -> Host {
        host.devices = vec![("SER-R".into(), "/dev/sdx".into())];
        host
    }

    fn cd_runner(extra: &[(&str, Reply)]) -> Arc<ScriptedRunner> {
        let mut rules: Vec<(&str, Reply)> = vec![("lsblk", Reply::Out(0, LSBLK_OK.into()))];
        rules.extend(extra.iter().cloned());
        ScriptedRunner::with_rules(&rules)
    }

    fn clear_drive_on(host: &Host, runner: &Arc<ScriptedRunner>, t: &Target) -> ClearDrive {
        host.with_clear(runner, |c| {
            host.with_probes(runner, |p| {
                clear_drive_with(&**runner, p, c, t, &Recorder::default())
            })
        })
    }

    #[test]
    fn clear_drive_line_and_exit_code_are_the_stop_policys() {
        assert_eq!(
            ClearDrive::Cleared("a".into()).line(),
            ("CLEAR DRIVE: OK  (a)".to_string(), 0)
        );
        assert_eq!(
            ClearDrive::Refused("b".into()).line(),
            ("CLEAR DRIVE: REFUSED  (b)".to_string(), 3)
        );
        assert_eq!(
            ClearDrive::CannotRun("c".into()).line(),
            ("CLEAR DRIVE: CANNOT-RUN  (c)".to_string(), 2)
        );
    }

    /// Review 5.
    #[test]
    fn clear_drive_unknown_serial_cannot_run() {
        let host = esp_mounted_by_udisks(None); // no drive attached
        let runner = cd_runner(&[]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(
            matches!(&got, ClearDrive::CannotRun(w) if w.contains("SER-R")),
            "{got:?}"
        );
        assert_eq!(got.line().1, 2);
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    #[test]
    fn clear_drive_two_serials_cannot_run() {
        let host = cd_host(esp_mounted_by_udisks(None));
        let runner = cd_runner(&[]);
        let mut t = recovery_target();
        t.serials = vec!["SER-R".into(), "SER-S".into()];

        let got = clear_drive_on(&host, &runner, &t);

        assert!(
            matches!(&got, ClearDrive::CannotRun(w) if w.contains("not a single-drive target")),
            "{got:?}"
        );
        assert!(umounts(&runner).is_empty());
    }

    #[test]
    fn clear_drive_lsblk_failure_cannot_run() {
        let host = cd_host(esp_mounted_by_udisks(None));
        let runner = ScriptedRunner::with_rules(&[("lsblk", Reply::exit(32))]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(
            matches!(&got, ClearDrive::CannotRun(w) if w.contains("lsblk")),
            "{got:?}"
        );
        assert!(umounts(&runner).is_empty());
    }

    /// A partition `lsblk` names that is not a block device (the drive went
    /// away, or a name that is something else): cannot answer — never "no
    /// mounts".
    #[test]
    fn clear_drive_a_partition_that_is_not_a_block_device_cannot_run() {
        let mut host = cd_host(esp_mounted_by_udisks(None));
        host.block_devs.retain(|(p, _)| p != "/dev/sdx2");
        let runner = cd_runner(&[]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(
            matches!(&got, ClearDrive::CannotRun(w) if w.contains("/dev/sdx2")),
            "{got:?}"
        );
        assert!(umounts(&runner).is_empty());
    }

    #[test]
    fn clear_drive_nothing_mounted_is_ok() {
        let mut host = cd_host(esp_mounted_by_udisks(None));
        host.mountinfo.truncate(1);
        let runner = cd_runner(&[]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(matches!(got, ClearDrive::Cleared(_)), "{got:?}");
        assert_eq!(got.line().1, 0);
        assert!(umounts(&runner).is_empty());
    }

    #[test]
    fn clear_drive_idle_esp_unmounted_ok() {
        let host = cd_host(esp_mounted_by_udisks(None));
        let runner = cd_runner(&[]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(matches!(got, ClearDrive::Cleared(_)), "{got:?}");
        assert_eq!(umounts(&runner), vec![ESP_AT.to_string()]);
        assert!(host.signals().is_empty());
    }

    #[test]
    fn clear_drive_reader_is_stopped_then_unmounted() {
        let host = cd_host(esp_mounted_by_udisks(Some(esp_user(300, "0100000"))));
        let runner = cd_runner(&[]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(
            matches!(&got, ClearDrive::Cleared(d) if d.contains("SIGTERM to pid 300")),
            "{got:?}"
        );
        assert_eq!(host.signals(), vec![(300, crate::holders::Sig::Term)]);
        assert_eq!(umounts(&runner), vec![ESP_AT.to_string()]);
    }

    #[test]
    fn clear_drive_writer_refused_exit_3() {
        let host = cd_host(esp_mounted_by_udisks(Some(esp_user(301, "0100002"))));
        let runner = cd_runner(&[]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(
            matches!(&got, ClearDrive::Refused(d) if d.contains("pid 301")),
            "{got:?}"
        );
        assert_eq!(got.line().1, 3);
        assert!(host.signals().is_empty());
        assert!(umounts(&runner).is_empty());
    }

    /// Review 3: the partition goes away under the unmount: never "OK".
    #[test]
    fn clear_drive_partition_gone_before_unmount_is_not_ok() {
        let host = cd_host(esp_mounted_by_udisks(None));
        let runner = cd_runner(&[("umount", Reply::exit(32))]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(
            matches!(&got, ClearDrive::Refused(d) if d.contains("will not unmount")),
            "{got:?}"
        );
        assert_eq!(umounts(&runner).len(), UMOUNT_ATTEMPTS as usize);
    }

    #[test]
    fn clear_drive_report_only_unmounts_and_signals_nothing() {
        let mut host = cd_host(esp_mounted_by_udisks(Some(esp_user(300, "0100000"))));
        host.report_only = true;
        let runner = cd_runner(&[]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(
            matches!(&got, ClearDrive::Cleared(d) if d.starts_with("dry run:") && d.contains("would unmount")),
            "{got:?}"
        );
        assert!(umounts(&runner).is_empty());
        assert!(host.signals().is_empty());
    }

    /// The whole disk is asked about too: a filesystem made on it directly.
    #[test]
    fn clear_drive_a_mount_of_the_whole_disk_is_cleared_too() {
        let mut host = cd_host(esp_mounted_by_udisks(None));
        host.mountinfo[1] = format!("50 22 8:16 / {ESP_AT} rw - vfat /dev/sdx rw");
        let runner = cd_runner(&[]);

        let got = clear_drive_on(&host, &runner, &recovery_target());

        assert!(matches!(got, ClearDrive::Cleared(_)), "{got:?}");
        assert_eq!(umounts(&runner), vec![ESP_AT.to_string()]);
    }

    /// The other program's mount is never bound: its reader is stopped, it
    /// is unmounted, and the target goes through the guarded mount at its
    /// own path; the mount, its origin, the holder and the signal are all
    /// said.
    #[test]
    fn a_raid1_target_mounted_by_another_program_is_cleared_then_mounted_through_the_guard() {
        let scratch = Scratch::new();
        let (config, host, runner, mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert!(!binds(&runner), "bound as found: {:?}", runner.calls());
        assert_eq!(host.signals(), vec![(300, crate::holders::Sig::Term)]);
        let calls = runner.calls();
        let umount = calls
            .iter()
            .position(|c| c == &status_call(&["umount", "--no-canonicalize", "/run/media/u/das"]))
            .expect("the other program's mount is unmounted");
        let mount = calls
            .iter()
            .position(|c| {
                c == &output_call(&[
                    "mount",
                    "-t",
                    "btrfs",
                    "-o",
                    "noatime,compress=zstd",
                    "UUID=u-p",
                    &mnt,
                ])
            })
            .expect("the guarded mount at the target's own path");
        assert!(umount < mount, "{calls:?}");
        assert_eq!(guard.count(), 1);
        // bd 3gzs.14: aligned rows, a kind word first.
        for row in [
            "mount    ",
            "  btrfs rw  /run/media/u/das  by udisks",
            "options  mount rw,nosuid  filesystem rw,subvolid=5,subvol=/  root /",
            "holder   pid 300  ",
            "signal   SIGTERM pid 300 (dolphin, user ",
            "action   unmounted /run/media/u/das",
        ] {
            assert!(logged(&progress, row), "{row}: {:?}", progress.logs());
        }
    }

    /// A job that writes nothing under the target (verify, walk, restore,
    /// browse, doctor) never stops the other program's reader: the target is
    /// refused, the reader named, nothing signalled or unmounted (operator
    /// 2026-11-25, bd su1b.12).
    #[test]
    fn a_read_only_job_refuses_another_programs_mount_with_a_reader_and_stops_nothing() {
        let scratch = Scratch::new();
        let (config, mut host, runner, mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        host.read_only_job = true;
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(host.signals().is_empty(), "{:?}", host.signals());
        let calls = runner.calls();
        assert!(
            !calls.iter().any(|c| c.argv[0] == "umount"
                || (c.argv[0] == "mount" && c.argv.iter().any(|a| a == &mnt))),
            "{calls:?}"
        );
        assert!(
            logged(&progress, "never stops a process") && logged(&progress, "pid 300"),
            "{:?}",
            progress.logs()
        );
    }

    /// The same for a busy leftover at the target's own path: a read-only
    /// job tries the plain unmounts, then refuses naming the holder, never a
    /// signal and never `umount -f`.
    #[test]
    fn a_read_only_job_never_stops_a_busy_leftovers_reader() {
        let scratch = Scratch::new();
        let (mut config, mut host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        scratch.mkdir("mnt/p/sub");
        let real_id = crate::holders::testproc::FakeProc::real_mount_id(Path::new(&mnt));
        host.mountinfo = vec![format!(
            "{real_id} 22 0:52 / {mnt} rw - btrfs /dev/sdb1 rw,subvolid=5"
        )];
        let proc = crate::holders::testproc::FakeProc::new();
        proc.add(crate::holders::testproc::Proc {
            pid: 310,
            cmd: "bash",
            cwd: Box::leak(format!("{mnt}/sub").into_boxed_str()),
            ..Default::default()
        });
        host.proc = Some(Arc::new(proc));
        host.read_only_job = true;
        let runner = ScriptedRunner::with_rules(&[
            (
                "UUID",
                Reply::Exit {
                    code: 0,
                    stdout: "u-p\n",
                    stderr: "",
                },
            ),
            ("umount", Reply::exit(32)),
        ]);
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        let err = format!("{:?}", result.as_ref().map(|g| g.count()));
        assert!(
            matches!(&result, Err(MountError::LeftoverMounted { detail, .. })
                if detail.contains("never stops a process") && detail.contains("pid 310 (bash)")),
            "{err}"
        );
        assert!(host.signals().is_empty(), "{:?}", host.signals());
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv.iter().any(|a| a == "-f")),
            "{:?}",
            runner.calls()
        );
        assert!(logged(&progress, "pid 310"), "{:?}", progress.logs());
    }

    /// What the clearing stopped is kept for the job's report, naming each
    /// process (pid, user, unit, command, signal), not only said in the log
    /// (bd su1b.31).
    #[test]
    fn the_processes_a_clearing_stopped_are_kept_for_the_report() {
        let scratch = Scratch::new();
        let (config, host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        let notes = guard.clear_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(notes[0].label, config.targets[0].label);
        assert_eq!(notes[0].outcome, ClearOutcome::Stopped);
        assert!(
            notes[0].detail.contains(
                "stopped what held it: SIGTERM to pid 300 (dolphin; user user, unit \
                 session-2.scope)"
            ),
            "{notes:?}"
        );
        assert!(!notes[0].is_fail());
    }

    /// A target skipped because of a writer is a FAIL note naming the
    /// writer, carried out through the error when nothing else mounted.
    #[test]
    fn a_target_skipped_for_a_writer_is_kept_for_the_report_naming_it() {
        let scratch = Scratch::new();
        let writer = crate::holders::testproc::Proc {
            pid: 301,
            cmd: "rsync",
            fds: vec![(4, "/run/media/u/das/big", "0100001", Some(77))],
            ..Default::default()
        };
        let (config, host, runner, _mnt) = raid1_mounted_by_udisks(&scratch, writer);

        let result = host.mount_targets(&config, &Recorder::default(), &runner);

        let Err(MountError::AllTargetsRefused { notes }) = result else {
            panic!("{:?}", result.err());
        };
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(notes[0].outcome, ClearOutcome::Refused);
        assert!(notes[0].is_fail());
        assert!(
            notes[0].detail.contains("pid 301") && notes[0].detail.contains("writing"),
            "{notes:?}"
        );
    }

    /// A dry run keeps what it would have stopped, as a note that is not a
    /// failure.
    #[test]
    fn a_dry_run_keeps_what_it_would_stop_as_a_note_that_does_not_fail() {
        let scratch = Scratch::new();
        let (config, mut host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        host.report_only = true;

        let result = host.mount_targets(&config, &Recorder::default(), &runner);

        let Err(MountError::AllTargetsRefused { notes }) = result else {
            panic!("{:?}", result.err());
        };
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(notes[0].outcome, ClearOutcome::WouldStop);
        assert!(!notes[0].is_fail());
        assert!(notes[0].detail.contains("would stop: pid 300"), "{notes:?}");
    }

    /// A process stopped to take down a busy leftover is kept too.
    #[test]
    fn the_processes_stopped_for_a_busy_leftover_are_kept_for_the_report() {
        let scratch = Scratch::new();
        let (mut config, mut host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        scratch.mkdir("mnt/p/sub");
        let real_id = crate::holders::testproc::FakeProc::real_mount_id(Path::new(&mnt));
        host.mountinfo = vec![format!(
            "{real_id} 22 0:52 / {mnt} rw - btrfs /dev/sdb1 rw,subvolid=5"
        )];
        let proc = crate::holders::testproc::FakeProc::new();
        proc.add(crate::holders::testproc::Proc {
            pid: 310,
            cmd: "bash",
            cwd: Box::leak(format!("{mnt}/sub").into_boxed_str()),
            ..Default::default()
        });
        host.proc = Some(Arc::new(proc));
        let runner = ScriptedRunner::with_rules(&[
            (
                "UUID",
                Reply::Exit {
                    code: 0,
                    stdout: "u-p\n",
                    stderr: "",
                },
            ),
            ("umount", Reply::Seq(&[32, 32, 32, 32, 32, 0])),
        ]);

        let guard = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .unwrap();

        let notes = guard.clear_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(notes[0].outcome, ClearOutcome::Stopped);
        assert!(
            notes[0].detail.contains("SIGTERM to pid 310 (bash;"),
            "{notes:?}"
        );
    }

    /// A process writing to it is never signalled: nothing of the other
    /// program's is touched, the target is NOT available, the writer named.
    #[test]
    fn a_raid1_target_another_program_is_writing_to_is_skipped_naming_the_writer() {
        let scratch = Scratch::new();
        let writer = crate::holders::testproc::Proc {
            pid: 301,
            cmd: "rsync",
            fds: vec![(4, "/run/media/u/das/big", "0100001", Some(77))],
            ..Default::default()
        };
        let (config, host, runner, mnt) = raid1_mounted_by_udisks(&scratch, writer);
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(host.signals().is_empty(), "{:?}", host.signals());
        assert!(!binds(&runner), "{:?}", runner.calls());
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| (c.argv[0] == "umount" || c.argv[0] == "mount")
                    && c.argv.iter().any(|a| a == "/run/media/u/das" || a == &mnt)),
            "{:?}",
            runner.calls()
        );
        assert!(
            progress.logs().iter().any(|(l, m)| *l == LogLevel::Warning
                && m.contains("pid 301")
                && m.contains("writing")
                && m.contains("NOT available")),
            "{:?}",
            progress.logs()
        );
    }

    /// Every mount this host shows is gone, yet the kernel still has the
    /// filesystem mounted: another mount namespace or a lazy unmount holds
    /// it. NOT available, never mounted beside it.
    #[test]
    fn a_raid1_target_still_mounted_in_another_namespace_is_skipped() {
        let scratch = Scratch::new();
        let (config, mut host, runner, mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        host.in_sysfs = vec!["u-p".into()];
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv[0] == "mount" && c.argv.iter().any(|a| a == &mnt)),
            "{:?}",
            runner.calls()
        );
        assert!(
            logged(&progress, "another mount namespace"),
            "{:?}",
            progress.logs()
        );
    }

    /// Under a handed-down hold nothing is cleared (the parent owns the run's
    /// mounts), and another program's mount of a RAID-1 target is still
    /// never bound: NOT available (bd su1b.12 review: the script's skip
    /// leaves a writer's mount up for a delegated child to find).
    #[test]
    fn a_delegated_job_never_binds_another_programs_mount_of_a_raid1_target() {
        let scratch = Scratch::new();
        let (config, host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        let progress = Recorder::default();

        let result = host.mount_targets_held(
            &config,
            &progress,
            &runner,
            &MaintenanceHeld::delegated_for_test(),
        );

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(!binds(&runner), "bound as found: {:?}", runner.calls());
        assert!(host.signals().is_empty(), "{:?}", host.signals());
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv[0] == "mount" || c.argv[0] == "umount"),
            "{:?}",
            runner.calls()
        );
        assert!(logged(&progress, "NOT available"), "{:?}", progress.logs());
    }

    /// A dry run never stops a holder nor unmounts another program's mount:
    /// it says what it would do, and the target is NOT available for it.
    #[test]
    fn a_dry_run_says_whom_it_would_stop_and_touches_nothing() {
        let scratch = Scratch::new();
        let (config, mut host, runner, mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        host.report_only = true;
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(host.signals().is_empty(), "{:?}", host.signals());
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| (c.argv[0] == "umount" || c.argv[0] == "mount")
                    && c.argv.iter().any(|a| a == "/run/media/u/das" || a == &mnt)),
            "{:?}",
            runner.calls()
        );
        assert!(
            logged(&progress, "holder   pid 300  ") && logged(&progress, "  -> would stop"),
            "{:?}",
            progress.logs()
        );
        assert!(
            logged(&progress, "action   would unmount /run/media/u/das"),
            "{:?}",
            progress.logs()
        );
    }

    /// btrfs maintenance running on it (balance, replace, device remove…)
    /// is never interrupted: the target is NOT available, nothing signalled.
    #[test]
    fn an_exclusive_btrfs_operation_on_it_refuses_the_clearing() {
        let scratch = Scratch::new();
        let (config, mut host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        host.exclusive = vec![("u-p".into(), "balance\n".into())];
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(host.signals().is_empty(), "{:?}", host.signals());
        assert!(
            !runner.calls().contains(&status_call(&[
                "umount",
                "--no-canonicalize",
                "/run/media/u/das"
            ])),
            "{:?}",
            runner.calls()
        );
        assert!(logged(&progress, "balance"), "{:?}", progress.logs());
    }

    /// A scrub running on it is never interrupted either.
    #[test]
    fn a_scrub_running_on_it_refuses_the_clearing() {
        let scratch = Scratch::new();
        let (config, host, _runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        let part_b = scratch.path("dev/sdb1");
        let part_c = scratch.path("dev/sdc1");
        let runner = ScriptedRunner::with_rules(&[
            ("blkid", Reply::Out(0, format!("{part_b}\n{part_c}\n"))),
            (
                "scrub",
                Reply::Out(
                    0,
                    "UUID:             u-p\nScrub started:    now\nStatus:           running\n"
                        .into(),
                ),
            ),
        ]);
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(host.signals().is_empty(), "{:?}", host.signals());
        assert!(logged(&progress, "scrub"), "{:?}", progress.logs());
    }

    /// Mounted nowhere this host can see, yet the kernel has it mounted:
    /// another mount namespace, or a lazy unmount held open. Never mounted
    /// beside it.
    #[test]
    fn a_raid1_target_mounted_only_where_this_host_cannot_see_is_skipped() {
        let scratch = Scratch::new();
        let (config, mut host, runner, mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        host.mountinfo.truncate(1);
        host.mounted_elsewhere.clear();
        host.in_sysfs = vec!["u-p".into()];
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(
            matches!(result, Err(MountError::AllTargetsRefused { .. })),
            "{:?}",
            result.err()
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv[0] == "mount" && c.argv.iter().any(|a| a == &mnt)),
            "{:?}",
            runner.calls()
        );
        assert!(
            logged(&progress, "another mount namespace"),
            "{:?}",
            progress.logs()
        );
    }

    /// A busy leftover at the target's own path: after the plain tries, its
    /// reader is stopped and a plain unmount releases it — no `umount -f`.
    #[test]
    fn a_busy_leftover_has_its_reader_stopped_before_any_forced_unmount() {
        let scratch = Scratch::new();
        let (mut config, mut host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        scratch.mkdir("mnt/p/sub");
        let real_id = crate::holders::testproc::FakeProc::real_mount_id(Path::new(&mnt));
        host.mountinfo = vec![format!(
            "{real_id} 22 0:52 / {mnt} rw - btrfs /dev/sdb1 rw,subvolid=5"
        )];
        let proc = crate::holders::testproc::FakeProc::new();
        proc.add(crate::holders::testproc::Proc {
            pid: 310,
            cmd: "bash",
            cwd: Box::leak(format!("{mnt}/sub").into_boxed_str()),
            ..Default::default()
        });
        host.proc = Some(Arc::new(proc));
        let runner = ScriptedRunner::with_rules(&[
            (
                "UUID",
                Reply::Exit {
                    code: 0,
                    stdout: "u-p\n",
                    stderr: "",
                },
            ),
            ("umount", Reply::Seq(&[32, 32, 32, 32, 32, 0])),
        ]);
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(host.signals(), vec![(310, crate::holders::Sig::Term)]);
        let calls = runner.calls();
        assert_eq!(
            calls[1..6],
            vec![status_call(&["umount", &mnt]); 5][..],
            "{calls:?}"
        );
        assert_eq!(calls[6], output_call(&["btrfs", "scrub", "status", &mnt]));
        assert_eq!(calls[7], status_call(&["umount", &mnt]), "{calls:?}");
        assert!(
            !calls.iter().any(|c| c.argv.iter().any(|a| a == "-f")),
            "{calls:?}"
        );
        assert_eq!(guard.count(), 1);
        assert!(
            logged(&progress, "signal   SIGTERM pid 310 (bash, user "),
            "{:?}",
            progress.logs()
        );
    }

    fn clear_cli(
        host: &Host,
        runner: &Arc<ScriptedRunner>,
        target: &Target,
        leftover: bool,
    ) -> ClearTarget {
        host.with_clear(runner, |c| {
            host.with_probes(runner, |p| {
                clear_target_with(&**runner, p, c, target, leftover, &Recorder::default())
            })
        })
    }

    /// `btrdasd backup clear-target` for the script: the same clearing as the
    /// Rust jobs, one line and exit code.
    #[test]
    fn clear_target_clears_another_programs_mount_for_the_script() {
        let scratch = Scratch::new();
        let (config, host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));

        let got = clear_cli(&host, &runner, &config.targets[0], false);

        assert!(matches!(got, ClearTarget::Cleared(_)), "{got:?}");
        assert_eq!(got.line().1, 0);
        assert!(got.line().0.starts_with("CLEAR TARGET: OK  ("), "{got:?}");
        assert_eq!(host.signals(), vec![(300, crate::holders::Sig::Term)]);
        assert!(runner.calls().contains(&status_call(&[
            "umount",
            "--no-canonicalize",
            "/run/media/u/das"
        ])));
        assert!(
            !runner.calls().iter().any(|c| c.argv[0] == "mount"),
            "clear-target mounts nothing: {:?}",
            runner.calls()
        );
    }

    #[test]
    fn clear_target_leftover_stops_its_holders_and_never_touches_what_is_not_the_target() {
        let scratch = Scratch::new();
        let (mut config, mut host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        host.mountinfo = vec![format!("88 22 0:52 / {mnt} rw - btrfs /dev/sdb1 rw")];
        let proc = crate::holders::testproc::FakeProc::new();
        proc.add(crate::holders::testproc::Proc {
            pid: 320,
            cmd: "less",
            fds: vec![(3, "/x", "0100000", Some(88))],
            ..Default::default()
        });
        host.proc = Some(Arc::new(proc));
        let uuid = |u: &'static str| {
            ScriptedRunner::with_rules(&[(
                "UUID",
                Reply::Exit {
                    code: 0,
                    stdout: u,
                    stderr: "",
                },
            )])
        };

        let other = uuid("someone-else\n");
        let got = clear_cli(&host, &other, &config.targets[0], true);
        assert!(matches!(got, ClearTarget::Refused(_)), "{got:?}");
        assert_eq!(got.line().1, 3);
        assert!(host.signals().is_empty());

        let ours = uuid("u-p\n");
        let got = clear_cli(&host, &ours, &config.targets[0], true);
        assert!(matches!(got, ClearTarget::Cleared(_)), "{got:?}");
        assert_eq!(host.signals(), vec![(320, crate::holders::Sig::Term)]);
        assert!(
            !ours.calls().iter().any(|c| c.argv[0] == "umount"),
            "the script unmounts it itself: {:?}",
            ours.calls()
        );
    }

    /// A leftover something is writing to is never forced or signalled: the
    /// job is refused naming the writer.
    #[test]
    fn a_leftover_held_open_for_writing_refuses_without_a_signal_or_a_forced_unmount() {
        let scratch = Scratch::new();
        let (mut config, mut host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        host.mountinfo = vec![format!(
            "88 22 0:52 / {mnt} rw - btrfs /dev/sdb1 rw,subvolid=5"
        )];
        let proc = crate::holders::testproc::FakeProc::new();
        proc.add(crate::holders::testproc::Proc {
            pid: 311,
            cmd: "cp",
            fds: vec![(5, "/x", "0100002", Some(88))],
            ..Default::default()
        });
        host.proc = Some(Arc::new(proc));
        let runner = ScriptedRunner::with_rules(&[
            (
                "UUID",
                Reply::Exit {
                    code: 0,
                    stdout: "u-p\n",
                    stderr: "",
                },
            ),
            ("umount", Reply::Seq(&[32])),
        ]);

        let err = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .err()
            .expect("refused");

        assert!(host.signals().is_empty());
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv.iter().any(|a| a == "-f"))
        );
        let e = err.to_string();
        assert!(e.contains("pid 311") && e.contains("writing"), "{e}");
    }

    /// Something other than the target at the target's path is never
    /// unmounted: it is not this project's to take down. Refused.
    #[test]
    fn a_foreign_filesystem_at_the_target_path_is_refused_never_unmounted() {
        let scratch = Scratch::new();
        let (mut config, mut host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        let runner = ScriptedRunner::with_rules(&[(
            "UUID",
            Reply::Exit {
                code: 0,
                stdout: "u-other\n",
                stderr: "",
            },
        )]);

        let err = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .err()
            .expect("a foreign mount refuses the job");

        assert_eq!(
            err.to_string(),
            format!(
                "Target 'primary': {mnt} is mounted but is not this target ('{mnt}' has \
                 filesystem UUID 'u-other', expected 'u-p') — refusing to run; it is not \
                 this job's to unmount"
            )
        );
        assert_eq!(
            runner.calls(),
            vec![output_call(&[
                "findmnt", "-n", "-o", "UUID", "--target", &mnt
            ])]
        );
    }

    /// A target with no `mount_uuid` is identified by its drive's serial; one
    /// that cannot be identified is refused, never unmounted.
    #[test]
    fn a_leftover_that_cannot_be_identified_is_refused_never_unmounted() {
        let scratch = Scratch::new();
        let a = scratch.mkdir("mnt/a");
        let config = config_with(
            vec![target("alpha", "SER-A", &a, TargetRole::Primary)],
            Vec::new(),
        );
        let host = Host {
            mountpoints: vec![PathBuf::from(&a)],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();

        let err = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .err()
            .expect("an unidentified mount refuses the job");

        assert!(
            err.to_string().starts_with(&format!(
                "Target 'alpha': {a} is mounted but is not this target ("
            )),
            "{err}"
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv[0] == "umount" || c.argv[0] == "mount"),
            "{:?}",
            runner.calls()
        );
    }

    /// Under a handed-down lock the parent (`backup-run.sh`) mounted the
    /// target for this run: an Info line, never the "left mounted" Warning
    /// (bd 8veh). The owned hold keeps its Warning (previous test).
    #[test]
    fn target_mounted_by_the_delegating_parent_is_info_not_warning() {
        let scratch = Scratch::new();
        let a = scratch.mkdir("mnt/a");
        let config = config_with(
            vec![target("alpha", "SER-A", &a, TargetRole::Primary)],
            Vec::new(),
        );
        let host = Host {
            mountpoints: vec![PathBuf::from(&a)],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = host
            .mount_targets_held(
                &config,
                &progress,
                &runner,
                &MaintenanceHeld::delegated_for_test(),
            )
            .unwrap();

        assert_eq!(guard.count(), 0);
        assert_eq!(
            progress.logs(),
            vec![(
                LogLevel::Info,
                format!(
                    "Target 'alpha': {a} is mounted by the job that handed down the \
                     maintenance lock; left mounted for it"
                )
            )]
        ); // Counted as the first of one target, not the zeroth.
        assert!(
            progress
                .steps()
                .contains(&(1, 1, "alpha already mounted".to_string())),
            "{:?}",
            progress.steps()
        );
    }

    /// A directory that merely EXISTS at the mount path is not a mounted
    /// target: it must be mounted, and the guard must own the unmount.
    #[test]
    fn target_with_a_bare_existing_directory_is_mounted_and_owned_by_the_guard() {
        let scratch = Scratch::new();
        let (config, host, part, mnt) = attached_primary(&scratch);
        scratch.mkdir("mnt/p");
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(guard.count(), 1);
        assert_eq!(
            runner.calls(),
            vec![output_call(&["mount", "-t", "btrfs", &part, &mnt])]
        );
        assert_eq!(
            progress.steps(),
            vec![(1, 1, format!("Mounted primary at {mnt}"))]
        );
        assert!(progress.logs().is_empty(), "{:?}", progress.logs());

        drop(guard);
        assert_eq!(runner.calls()[1..], [status_call(&["umount", &mnt])]);
    }

    /// `[das].mount_opts` (it carries `degraded` for the RAID-1 primary) is
    /// passed through, and a mirror target mounts partition 2 — partition 1
    /// there is the recovery ESP.
    #[test]
    fn target_mount_passes_configured_options_and_uses_the_role_partition() {
        let scratch = Scratch::new();
        scratch.touch("dev/sdc1");
        let part = scratch.touch("dev/sdc2");
        let mnt = scratch.path("mnt/recovery");
        let mut config = config_with(
            vec![target("recovery-A", "SER-M", &mnt, TargetRole::Mirror)],
            Vec::new(),
        );
        config.das.mount_opts = "degraded,noatime".into();
        let host = Host {
            devices: vec![("SER-M".into(), scratch.path("dev/sdc"))],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();

        let guard = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .unwrap();

        assert_eq!(guard.count(), 1);
        assert_eq!(
            runner.calls(),
            vec![output_call(&[
                "mount",
                "-t",
                "btrfs",
                "-o",
                "degraded,noatime",
                &part,
                &mnt
            ])]
        );
    }

    #[test]
    fn target_mount_point_is_created_when_missing() {
        let scratch = Scratch::new();
        let (config, host, _part, mnt) = attached_primary(&scratch);
        let runner = ScriptedRunner::succeeding();
        assert!(!Path::new(&mnt).exists());

        let guard = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .unwrap();

        assert!(Path::new(&mnt).is_dir(), "mount point must be created");
        assert_eq!(guard.count(), 1);
    }

    #[test]
    fn target_whose_mount_point_cannot_be_created_is_skipped_with_a_warning() {
        let scratch = Scratch::new();
        let part = scratch.touch("dev/sdb1");
        // A regular file where a parent directory is needed.
        let blocker = scratch.touch("blocker");
        let mnt = format!("{blocker}/p");
        let config = config_with(
            vec![target("primary", "SER-P", &mnt, TargetRole::Primary)],
            Vec::new(),
        );
        let host = Host {
            devices: vec![("SER-P".into(), scratch.path("dev/sdb"))],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        progress.assert_only_log(
            LogLevel::Warning,
            &["Target 'primary': could not create", &mnt, "skipping"],
        );
        assert!(Path::new(&part).exists());
    }

    /// A failed mount must never leave the target looking available: not in
    /// the guard, not as a progress step, and — when it was the only target —
    /// not as an `Ok` (bd DAS-Backup-Manager-aea).
    #[test]
    fn target_whose_mount_fails_is_not_available() {
        let scratch = Scratch::new();
        let (config, host, part, mnt) = attached_primary(&scratch);
        let runner = ScriptedRunner::with_rules(&[(
            &mnt,
            Reply::Exit {
                code: 32,
                stdout: "",
                stderr: "mount: wrong fs type, bad superblock\n",
            },
        )]);
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert_eq!(
            runner.calls(),
            vec![output_call(&["mount", "-t", "btrfs", &part, &mnt])],
            "nothing was mounted, so nothing may be unmounted"
        );
        assert!(progress.steps().is_empty(), "{:?}", progress.steps());
        assert_eq!(
            progress.logs(),
            vec![(
                LogLevel::Warning,
                format!(
                    "Target 'primary': mount {part} → {mnt} failed: \
                     mount: wrong fs type, bad superblock"
                )
            )]
        );
    }

    #[test]
    fn target_whose_mount_cannot_be_executed_is_not_available() {
        let scratch = Scratch::new();
        let (config, host, _part, mnt) = attached_primary(&scratch);
        let runner = ScriptedRunner::with_rules(&[(&mnt, Reply::CannotRun)]);
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert!(progress.steps().is_empty());
        progress.assert_only_log(
            LogLevel::Warning,
            &["Target 'primary': failed to execute mount:"],
        );
    }

    /// One target failing does not abort the others, and the guard ends up
    /// owning exactly the mounts that succeeded.
    #[test]
    fn a_failed_target_does_not_take_a_working_one_down_with_it() {
        let scratch = Scratch::new();
        scratch.touch("dev/sdb1");
        let good_part = scratch.touch("dev/sdc2");
        let bad_mnt = scratch.path("mnt/bad");
        let good_mnt = scratch.path("mnt/good");
        let config = config_with(
            vec![
                target("bad", "SER-BAD", &bad_mnt, TargetRole::Primary),
                target("good", "SER-GOOD", &good_mnt, TargetRole::Mirror),
            ],
            Vec::new(),
        );
        let host = Host {
            devices: vec![
                ("SER-BAD".into(), scratch.path("dev/sdb")),
                ("SER-GOOD".into(), scratch.path("dev/sdc")),
            ],
            ..Host::default()
        };
        let runner = ScriptedRunner::with_rules(&[(&bad_mnt, Reply::exit(32))]);
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(guard.count(), 1);
        assert_eq!(
            progress.steps(),
            vec![(2, 2, format!("Mounted good at {good_mnt}"))]
        );
        progress.assert_only_log(LogLevel::Warning, &["Target 'bad': mount", "failed"]);

        let mounts = runner.calls().len();
        assert_eq!(
            runner.calls()[1],
            output_call(&["mount", "-t", "btrfs", &good_part, &good_mnt])
        );
        drop(guard);
        assert_eq!(
            runner.calls()[mounts..],
            [status_call(&["umount", &good_mnt])]
        );
    }

    /// `target_attached` answers what the mount loop decides: a drive whose
    /// serial or `mount_uuid` is found is attached; one with neither is not —
    /// and the answer mounts nothing (the doctor's delivery check reads it).
    #[test]
    fn a_target_is_attached_only_when_its_serial_or_uuid_is_found() {
        let scratch = Scratch::new();
        let mnt = scratch.path("mnt/p");
        let gone = target("primary", "SER-GONE", &mnt, TargetRole::Primary);
        let mut by_uuid = gone.clone();
        by_uuid.mount_uuid = Some("5eed0016-0000-4000-8000-000000000016".into());
        let runner = ScriptedRunner::succeeding();
        let host = Host {
            uuids: vec![(
                "5eed0016-0000-4000-8000-000000000016".into(),
                "/dev/sdz".into(),
            )],
            ..Host::default()
        };
        host.with_probes(&runner, |probes| {
            assert!(!target_attached_with(&gone, probes), "no serial, no uuid");
            assert!(
                target_attached_with(&by_uuid, probes),
                "its uuid is on the host"
            );
        });
        let absent = Host::default();
        absent.with_probes(&runner, |probes| {
            assert!(
                !target_attached_with(&by_uuid, probes),
                "uuid not on the host"
            );
        });
        assert!(runner.calls().is_empty(), "asking mounts nothing");
    }

    #[test]
    fn target_whose_drive_is_absent_is_skipped_without_mounting() {
        let scratch = Scratch::new();
        let mnt = scratch.path("mnt/p");
        let config = config_with(
            vec![target("primary", "SER-GONE", &mnt, TargetRole::Primary)],
            Vec::new(),
        );
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let result = Host::default().mount_targets(&config, &progress, &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert!(runner.calls().is_empty());
        progress.assert_only_log(
            LogLevel::Warning,
            &["Target 'primary': drive serial 'SER-GONE' not found"],
        );
        assert!(
            !Path::new(&mnt).exists(),
            "no bare mount point may be left behind for an absent drive"
        );
    }

    /// A target identified by `mount_uuid` only: no configured serial is
    /// present (a loop device, a replacement drive), but the filesystem is —
    /// it is mounted BY UUID, as backup-run.sh does (bd DAS-Backup-Manager-arx).
    #[test]
    fn a_target_with_only_its_uuid_present_is_mounted_by_uuid() {
        let scratch = Scratch::new();
        let mnt = scratch.path("mnt/t");
        let mut t = target_at("loop", &mnt, Some("u-loop"));
        t.serial = String::new();
        t.serials = Vec::new();
        let mut config = config_with(vec![t], Vec::new());
        config.das.mount_opts = "noatime,degraded".into();
        let host = Host {
            uuids: vec![("u-loop".into(), "/dev/loop7".into())],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(
            sans_blkid(&runner),
            vec![output_call(&[
                "mount",
                "-t",
                "btrfs",
                "-o",
                "noatime,degraded",
                "UUID=u-loop",
                &mnt
            ])]
        );
        assert_eq!(guard.count(), 1, "the guard owns what it mounted");
        assert_eq!(
            progress.steps(),
            vec![(1, 1, format!("Mounted loop at {mnt} (UUID=u-loop)"))]
        );
        assert!(progress.logs().is_empty(), "{:?}", progress.logs());
    }

    /// With a serial present the UUID need not resolve on its own, and the
    /// mount is still by UUID (a degraded RAID-1 assembles from either leg).
    #[test]
    fn a_target_with_a_uuid_and_a_present_serial_is_mounted_by_uuid() {
        let scratch = Scratch::new();
        let (mut config, host, _part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(
            sans_blkid(&runner),
            vec![output_call(&["mount", "-t", "btrfs", "UUID=u-p", &mnt])]
        );
        assert_eq!(guard.count(), 1);
    }

    /// `btrfs inspect-internal dump-super` of one member of a two-device
    /// filesystem, as btrfs-progs prints it (first token the key).
    fn super_of(fsid: &str, devid: u64, dev_uuid: &str, generation: u64) -> String {
        format!(
            "superblock: bytenr=65536, device=/dev/x\n\
             ---------------------------------------------------------\n\
             csum_type\t\t0 (crc32c)\n\
             magic\t\t\t_BHRfS_M [match]\n\
             fsid\t\t\t{fsid}\n\
             metadata_uuid\t\t00000000-0000-0000-0000-000000000000\n\
             generation\t\t{generation}\n\
             chunk_root_generation\t{generation}\n\
             num_devices\t\t2\n\
             cache_generation\t0\n\
             uuid_tree_generation\t{generation}\n\
             dev_item.uuid\t\t{dev_uuid}\n\
             dev_item.fsid\t\t{fsid} [match]\n\
             dev_item.devid\t\t{devid}\n\
             dev_item.generation\t0\n"
        )
    }

    /// A RAID-1 target (a filesystem UUID and two serials) with both drives
    /// attached is mounted WITHOUT `degraded`, after each member found by its
    /// filesystem UUID was registered with btrfs by its own path — never an
    /// unscoped scan (bd DAS-Backup-Manager-su1b.10: a kernel that had
    /// forgotten one leg let `mount -o degraded` come up on the other).
    #[test]
    fn a_raid1_target_with_both_drives_attached_is_scanned_and_mounted_without_degraded() {
        let scratch = Scratch::new();
        let (mut config, mut host, part_b, mnt) = attached_primary(&scratch);
        let part_c = scratch.touch("dev/sdc1");
        config.targets[0].mount_uuid = Some("u-p".into());
        config.targets[0].serials = vec!["SER-P".into(), "SER-Q".into()];
        config.das.mount_opts = "noatime,degraded,compress=zstd".into();
        host.devices.push(("SER-Q".into(), scratch.path("dev/sdc")));
        let runner = ScriptedRunner::with_rules(&[
            ("blkid", Reply::Out(0, format!("{part_b}\n{part_c}\n"))),
            (
                &part_b,
                Reply::Out(
                    0,
                    super_of("u-p", 1, "5eed00a1-0000-4000-8000-0000000000a1", 40),
                ),
            ),
            (
                &part_c,
                Reply::Out(
                    0,
                    super_of("u-p", 2, "5eed00a2-0000-4000-8000-0000000000a2", 40),
                ),
            ),
        ]);
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        let calls = runner.calls();
        for part in [&part_b, &part_c] {
            assert!(
                calls.contains(&output_call(&["btrfs", "device", "scan", part])),
                "{part} not scanned by path: {calls:?}"
            );
        }
        assert!(
            calls.contains(&output_call(&[
                "mount",
                "-t",
                "btrfs",
                "-o",
                "noatime,compress=zstd",
                "UUID=u-p",
                &mnt
            ])),
            "{calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.argv[0] == "mount" && c.argv.iter().any(|a| a.contains("degraded"))),
            "mounted degraded with both members attached: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.argv == ["btrfs", "device", "scan"]
                    || c.argv.iter().any(|a| a == "--forget")),
            "an unscoped scan or a forget: {calls:?}"
        );
        assert_eq!(guard.count(), 1);
    }

    /// bd DAS-Backup-Manager-su1b.14: a member that drops while the job has
    /// the target mounted is recorded when the job gives the target back,
    /// before the unmount; a whole pair records nothing.
    #[test]
    fn a_raid1_member_that_drops_during_the_job_is_recorded_at_its_unmount() {
        for dropped in [false, true] {
            let scratch = Scratch::new();
            let (mut config, mut host, part_b, mnt) = attached_primary(&scratch);
            let part_c = scratch.touch("dev/sdc1");
            config.targets[0].mount_uuid = Some("u-p".into());
            config.targets[0].serials = vec!["SER-P".into(), "SER-Q".into()];
            config.das.mount_opts = "noatime,degraded".into();
            host.devices.push(("SER-Q".into(), scratch.path("dev/sdc")));
            host.raid_dir = Some(PathBuf::from(scratch.path("raid")));
            if dropped {
                host.missing_at_end = vec![("u-p".into(), vec![2])];
            }
            let dx = "5eed00a1-0000-4000-8000-0000000000a1";
            let runner = ScriptedRunner::with_rules(&[
                ("blkid", Reply::Out(0, format!("{part_b}\n{part_c}\n"))),
                (&part_b, Reply::Out(0, super_of("u-p", 1, dx, 40))),
                (
                    &part_c,
                    Reply::Out(
                        0,
                        super_of("u-p", 2, "5eed00a2-0000-4000-8000-0000000000a2", 40),
                    ),
                ),
            ]);
            let progress = Recorder::default();
            let mut guard = host.mount_targets(&config, &progress, &runner).unwrap();
            assert!(
                !std::path::Path::new(&scratch.path("raid/u-p.record")).exists(),
                "a whole mount records nothing"
            );
            let still = guard.unmount(&progress);
            assert!(still.is_empty(), "{still:?}");
            let logs = progress.logs.lock().unwrap().clone();
            let record = std::fs::read_to_string(scratch.path("raid/u-p.record"));
            if dropped {
                let record = record.expect("recorded");
                assert!(record.contains(&format!("member=1 {dx} 40")), "{record}");
                assert!(!record.contains("member=2"), "{record}");
                assert!(
                    std::fs::read_to_string(raid_guard::witness_path(&mnt))
                        .unwrap()
                        .contains(dx),
                    "the witness is written before the unmount"
                );
                assert!(
                    logs.iter().any(|(l, m)| matches!(l, LogLevel::Warning)
                        && m.contains("RAID CHECK: RECORDED")),
                    "{logs:?}"
                );
            } else {
                assert!(record.is_err(), "{record:?}");
                assert!(
                    logs.iter().any(|(_, m)| m.contains("RAID CHECK: OK")),
                    "{logs:?}"
                );
            }
        }
    }

    /// A mount the guard made degraded shows its absent member missing for
    /// its whole life; the period is already open and recorded at the mount,
    /// so the end-of-job check is not run on it (no WARN every degraded run).
    #[test]
    fn a_degraded_raid1_mount_is_not_rechecked_at_its_unmount() {
        let scratch = Scratch::new();
        let (mut config, mut host, part_b, _mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        config.targets[0].serials = vec!["SER-P".into(), "SER-Q".into()];
        config.das.mount_opts = "noatime,degraded".into();
        config.das.member_wait_secs = 0;
        host.raid_dir = Some(PathBuf::from(scratch.path("raid")));
        host.missing_at_end = vec![("u-p".into(), vec![2])];
        let runner = ScriptedRunner::with_rules(&[
            ("blkid", Reply::Out(0, format!("{part_b}\n"))),
            (
                &part_b,
                Reply::Out(
                    0,
                    super_of("u-p", 1, "5eed00a1-0000-4000-8000-0000000000a1", 40),
                ),
            ),
        ]);
        let progress = Recorder::default();
        let mut guard = host.mount_targets(&config, &progress, &runner).unwrap();
        let record = std::fs::read_to_string(scratch.path("raid/u-p.record")).unwrap();
        assert!(guard.unmount(&progress).is_empty());
        let logs = progress.logs.lock().unwrap().clone();
        assert!(
            !logs.iter().any(|(_, m)| m.contains("RAID CHECK")),
            "{logs:?}"
        );
        assert_eq!(
            std::fs::read_to_string(scratch.path("raid/u-p.record")).unwrap(),
            record,
            "the record as the mount left it"
        );
    }

    /// One leg absent after the wait: mounted degraded, the period recorded
    /// under the guard's record directory. The other leg alone on a later run
    /// is a split brain: the whole job is refused, never a skipped target.
    #[test]
    fn a_raid1_target_on_one_leg_mounts_degraded_and_the_other_leg_alone_is_refused() {
        let scratch = Scratch::new();
        let (mut config, mut host, part_b, mnt) = attached_primary(&scratch);
        let part_c = scratch.touch("dev/sdc1");
        config.targets[0].mount_uuid = Some("u-p".into());
        config.targets[0].serials = vec!["SER-P".into(), "SER-Q".into()];
        config.das.mount_opts = "noatime,degraded".into();
        config.das.member_wait_secs = 10;
        host.raid_dir = Some(PathBuf::from(scratch.path("raid")));
        let dx = "5eed00a1-0000-4000-8000-0000000000a1";
        let dy = "5eed00a2-0000-4000-8000-0000000000a2";
        let runner = ScriptedRunner::with_rules(&[
            ("blkid", Reply::Out(0, format!("{part_b}\n"))),
            (&part_b, Reply::Out(0, super_of("u-p", 1, dx, 40))),
        ]);
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(guard.count(), 1);
        let calls = runner.calls();
        assert!(
            calls.contains(&output_call(&[
                "mount",
                "-t",
                "btrfs",
                "-o",
                "noatime,degraded",
                "UUID=u-p",
                &mnt
            ])),
            "{calls:?}"
        );
        assert_eq!(
            calls.iter().filter(|c| c.argv[0] == "blkid").count(),
            4,
            "once for its mounts elsewhere (bd su1b.12), then once, and after each 5 s pause of the 10 s wait"
        );
        let record = std::fs::read_to_string(scratch.path("raid/u-p.record")).unwrap();
        assert!(record.contains(&format!("member=1 {dx} 40")), "{record}");
        assert!(
            std::fs::read_to_string(raid_guard::witness_path(&mnt))
                .unwrap()
                .contains(dx),
            "the witness is on the filesystem"
        );
        drop(guard);

        // The other leg alone.
        host.devices = vec![("SER-Q".into(), scratch.path("dev/sdc"))];
        let runner = ScriptedRunner::with_rules(&[
            ("blkid", Reply::Out(0, format!("{part_c}\n"))),
            (&part_c, Reply::Out(0, super_of("u-p", 2, dy, 38))),
        ]);
        std::fs::remove_file(raid_guard::witness_path(&mnt)).unwrap();
        let err = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .err()
            .expect("refused");
        assert!(
            matches!(&err, MountError::RaidRefused { detail, .. } if detail.contains("split brain")),
            "{err}"
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.argv[0] == "mount" && c.argv.iter().any(|a| a == "noatime,degraded")),
            "the stale leg was mounted writable: {:?}",
            runner.calls()
        );

        // A job that only reads (walk, restore) is given it read-only, and
        // the record is left as it was (bd DAS-Backup-Manager-su1b.11).
        let record_before = std::fs::read_to_string(scratch.path("raid/u-p.record")).unwrap();
        host.intent = Some(raid_guard::Intent::ReadOnly);
        let runner = ScriptedRunner::with_rules(&[
            ("blkid", Reply::Out(0, format!("{part_c}\n"))),
            (&part_c, Reply::Out(0, super_of("u-p", 2, dy, 38))),
        ]);
        let guard = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .expect("mounted read-only");
        assert_eq!(guard.count(), 1);
        assert!(
            runner.calls().contains(&output_call(&[
                "mount",
                "-t",
                "btrfs",
                "-o",
                "noatime,ro,degraded,rescue=nologreplay",
                "UUID=u-p",
                &mnt
            ])),
            "{:?}",
            runner.calls()
        );
        assert_eq!(
            std::fs::read_to_string(scratch.path("raid/u-p.record")).unwrap(),
            record_before
        );
        assert!(!raid_guard::witness_path(&mnt).exists(), "nothing written");
    }

    /// Any configured serial counts, not only the first — a RAID-1 pair with
    /// its first member gone is still found by its second.
    #[test]
    fn the_second_serial_of_a_pair_is_enough() {
        let scratch = Scratch::new();
        let (mut config, mut host, part, mnt) = attached_primary(&scratch);
        config.targets[0].serial = "SER-GONE".into();
        config.targets[0].serials = vec!["SER-GONE".into(), "SER-P".into()];
        host.devices = vec![("SER-P".into(), scratch.path("dev/sdb"))];
        let runner = ScriptedRunner::succeeding();

        let _guard = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .unwrap();

        assert_eq!(
            runner.calls(),
            vec![output_call(&["mount", "-t", "btrfs", &part, &mnt])]
        );
    }

    /// Neither a configured serial nor the UUID: skipped with a warning that
    /// names both, nothing created or mounted, and NoDrivesFound when it was
    /// the only target.
    #[test]
    fn a_target_with_neither_its_uuid_nor_a_serial_present_is_skipped() {
        let scratch = Scratch::new();
        let mnt = scratch.path("mnt/t");
        let config = config_with(vec![target_at("gone", &mnt, Some("u-gone"))], Vec::new());
        let host = Host {
            uuids: vec![("u-other".into(), "/dev/sdz1".into())],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert!(sans_blkid(&runner).is_empty());
        progress.assert_only_log(
            LogLevel::Warning,
            &[
                "Target 'gone': no configured drive (serials TESTSERIAL) is present",
                "no filesystem has UUID u-gone — skipping",
            ],
        );
        assert!(!Path::new(&mnt).exists());
    }

    #[test]
    fn a_uuid_target_with_no_serials_says_so_when_skipped() {
        let scratch = Scratch::new();
        let mut t = target_at("bare", &scratch.path("mnt/t"), Some("u-x"));
        t.serial = String::new();
        t.serials = Vec::new();
        let progress = Recorder::default();
        let result = Host::default().mount_targets(
            &config_with(vec![t], Vec::new()),
            &progress,
            &ScriptedRunner::succeeding(),
        );
        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        progress.assert_only_log(LogLevel::Warning, &["(serials none)"]);
    }

    /// The UUID mount fails but a configured drive is present: its partition
    /// is mounted instead, as backup-run.sh falls back.
    #[test]
    fn a_failed_uuid_mount_falls_back_to_the_serial_partition() {
        let scratch = Scratch::new();
        let (mut config, host, part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        let runner =
            ScriptedRunner::with_rules(&[("blkid", Reply::ok()), ("UUID=u-p", Reply::exit(32))]);
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(
            sans_blkid(&runner),
            vec![
                output_call(&["mount", "-t", "btrfs", "UUID=u-p", &mnt]),
                output_call(&["mount", "-t", "btrfs", &part, &mnt]),
            ]
        );
        assert_eq!(guard.count(), 1);
        let logs = progress.logs();
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert!(logs[0].1.contains("mount UUID=u-p"), "{logs:?}");
        assert!(
            logs[1].1.contains("falling back to the device-based mount"),
            "{logs:?}"
        );
    }

    /// The UUID mount fails and there is nothing to fall back to: the target
    /// is not available and the mount point created for it is removed.
    #[test]
    fn a_failed_uuid_mount_with_no_drive_leaves_the_target_unavailable() {
        let scratch = Scratch::new();
        let mnt = scratch.path("mnt/t");
        let config = config_with(vec![target_at("loop", &mnt, Some("u-loop"))], Vec::new());
        let host = Host {
            uuids: vec![("u-loop".into(), "/dev/loop7".into())],
            ..Host::default()
        };
        let runner =
            ScriptedRunner::with_rules(&[("blkid", Reply::ok()), ("UUID=u-loop", Reply::exit(32))]);

        let result = host.mount_targets(&config, &Recorder::default(), &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert_eq!(sans_blkid(&runner).len(), 1);
        assert!(!Path::new(&mnt).exists());
    }

    /// A mirror (recovery drive) found by UUID is mounted by UUID — the
    /// filesystem, never partition 1, which is its own OS's ESP.
    #[test]
    fn a_mirror_found_by_uuid_is_mounted_by_uuid_not_by_partition() {
        let scratch = Scratch::new();
        let mnt = scratch.path("mnt/recovery");
        let mut t = target("recovery-A", "SER-M", &mnt, TargetRole::Mirror);
        t.mount_uuid = Some("u-rec".into());
        let host = Host {
            uuids: vec![("u-rec".into(), "/dev/sdc2".into())],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();

        let _guard = host
            .mount_targets(
                &config_with(vec![t], Vec::new()),
                &Recorder::default(),
                &runner,
            )
            .unwrap();

        assert_eq!(
            sans_blkid(&runner),
            vec![output_call(&["mount", "-t", "btrfs", "UUID=u-rec", &mnt])]
        );
    }

    /// An empty `mount_uuid` is no UUID: the legacy serial route.
    #[test]
    fn an_empty_mount_uuid_is_the_serial_route() {
        let scratch = Scratch::new();
        let (mut config, host, part, mnt) = attached_primary(&scratch);
        config.targets[0].mount_uuid = Some(String::new());
        let runner = ScriptedRunner::succeeding();
        let _guard = host
            .mount_targets(&config, &Recorder::default(), &runner)
            .unwrap();
        assert_eq!(
            runner.calls(),
            vec![output_call(&["mount", "-t", "btrfs", &part, &mnt])]
        );
    }

    #[test]
    fn target_whose_partition_is_missing_is_skipped_without_mounting() {
        let scratch = Scratch::new();
        // The disk is there and has a first partition, but this is a mirror
        // target, which needs the second.
        scratch.touch("dev/sdc1");
        let mnt = scratch.path("mnt/recovery");
        let config = config_with(
            vec![target("recovery-A", "SER-M", &mnt, TargetRole::Mirror)],
            Vec::new(),
        );
        let host = Host {
            devices: vec![("SER-M".into(), scratch.path("dev/sdc"))],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        progress.assert_only_log(
            LogLevel::Warning,
            &[
                "Target 'recovery-A': partition",
                &scratch.path("dev/sdc2"),
                "not found",
            ],
        );
    }

    /// A bind mount takes the other program's mount as it is: `[das].mount_opts`
    /// cannot be applied, and that is said, never silently dropped (bd
    /// DAS-Backup-Manager-804; fail-silent.md "accepted and ignored").
    #[test]
    fn a_bind_mount_says_the_configured_mount_options_were_not_applied() {
        let scratch = Scratch::new();
        let mnt = scratch.path("mnt/p");
        let mut config = config_with(
            vec![target("primary", "SER-P", &mnt, TargetRole::Primary)],
            Vec::new(),
        );
        config.das.mount_opts = "noatime,compress=zstd,degraded".into();
        let host = Host {
            mounted_elsewhere: vec![("SER-P".into(), "/run/media/u/das".into())],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(guard.count(), 1);
        let logs = progress.logs();
        assert!(
            logs.iter().any(|(level, l)| *level == LogLevel::Warning
                && l.contains("noatime,compress=zstd,degraded")
                && l.contains("not applied")
                && l.contains("/run/media/u/das")),
            "{logs:?}"
        );
    }

    /// A target mounted somewhere else (udisks under /run/media) is
    /// bind-mounted to its configured path, and that bind mount is the
    /// guard's to undo.
    #[test]
    fn target_mounted_elsewhere_is_bind_mounted_at_the_configured_path() {
        let scratch = Scratch::new();
        let mnt = scratch.path("mnt/p");
        let config = config_with(
            vec![target("primary", "SER-P", &mnt, TargetRole::Primary)],
            Vec::new(),
        );
        let host = Host {
            mounted_elsewhere: vec![("SER-P".into(), "/run/media/u/das".into())],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert!(Path::new(&mnt).is_dir(), "bind target must be created");
        assert_eq!(guard.count(), 1);
        assert_eq!(
            runner.calls(),
            vec![status_call(&["mount", "--bind", "/run/media/u/das", &mnt])]
        );
        assert_eq!(
            progress.steps(),
            vec![(
                1,
                1,
                format!("primary bind-mounted /run/media/u/das → {mnt}")
            )]
        );
        assert!(progress.logs().is_empty(), "{:?}", progress.logs());

        drop(guard);
        assert_eq!(runner.calls()[1..], [status_call(&["umount", &mnt])]);
    }

    /// The bind-mount failure branch once marked the target available
    /// (bd DAS-Backup-Manager-aea). Both ways of failing must leave it out.
    #[test]
    fn target_whose_bind_mount_fails_is_not_available() {
        for reply in [Reply::exit(1), Reply::CannotRun] {
            let scratch = Scratch::new();
            let mnt = scratch.path("mnt/p");
            let config = config_with(
                vec![target("primary", "SER-P", &mnt, TargetRole::Primary)],
                Vec::new(),
            );
            let host = Host {
                mounted_elsewhere: vec![("SER-P".into(), "/run/media/u/das".into())],
                ..Host::default()
            };
            let runner = ScriptedRunner::with_rules(&[(&mnt, reply)]);
            let progress = Recorder::default();

            let result = host.mount_targets(&config, &progress, &runner);

            assert!(matches!(result, Err(MountError::NoDrivesFound)));
            assert_eq!(
                runner.calls().len(),
                1,
                "only the bind attempt — no unmount of a mount that never happened"
            );
            assert!(progress.steps().is_empty(), "{:?}", progress.steps());
            progress.assert_only_log(
                LogLevel::Warning,
                &["primary: bind mount /run/media/u/das", "NOT available"],
            );
        }
    }

    #[test]
    fn target_whose_bind_mount_point_cannot_be_created_is_not_available() {
        let scratch = Scratch::new();
        let blocker = scratch.touch("blocker");
        let mnt = format!("{blocker}/p");
        let config = config_with(
            vec![target("primary", "SER-P", &mnt, TargetRole::Primary)],
            Vec::new(),
        );
        let host = Host {
            mounted_elsewhere: vec![("SER-P".into(), "/run/media/u/das".into())],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        progress.assert_only_log(
            LogLevel::Warning,
            &[
                "Target 'primary': could not create mount point",
                "for bind mount",
                "NOT available",
            ],
        );
    }

    // -----------------------------------------------------------------------
    // ensure_sources_mounted
    // -----------------------------------------------------------------------

    /// Two sources on one volume mean ONE mount, attributed to the first, and
    /// the snapshot directory btrbk needs is created inside it.
    #[test]
    fn sources_sharing_a_volume_are_mounted_once() {
        let scratch = Scratch::new();
        let vol = scratch.path("vol/hdd");
        let config = config_with(
            Vec::new(),
            vec![
                source("hdd-projects", "/dev/fake-hdd", &vol),
                source("hdd-audiobooks", "/dev/fake-hdd", &vol),
            ],
        );
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = Host::default().mount_sources(&config, &progress, &runner);

        assert_eq!(guard.count(), 1);
        assert_eq!(
            runner.calls(),
            vec![output_call(&[
                "mount",
                "-o",
                "subvolid=5",
                "/dev/fake-hdd",
                &vol
            ])]
        );
        assert_eq!(
            progress.logs(),
            vec![(
                LogLevel::Info,
                format!("Source 'hdd-projects': mounted /dev/fake-hdd at {vol} (subvolid=5)")
            )]
        );
        assert!(Path::new(&vol).join(".btrbk-snapshots").is_dir());

        drop(guard);
        assert_eq!(runner.calls()[1..], [status_call(&["umount", &vol])]);
    }

    /// Giving source volumes back is reported as unmounting sources, not targets.
    #[test]
    fn releasing_source_volumes_is_staged_as_sources() {
        let scratch = Scratch::new();
        let vol = scratch.path("vol/hdd");
        let config = config_with(Vec::new(), vec![source("hdd", "/dev/fake-hdd", &vol)]);
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();
        let mut guard = Host::default().mount_sources(&config, &progress, &runner);

        assert!(guard.unmount(&progress).is_empty());

        assert_eq!(progress.stages(), vec![("Unmounting sources".into(), 1)]);
    }

    #[test]
    fn source_volume_already_mounted_is_left_alone() {
        let scratch = Scratch::new();
        let vol = scratch.mkdir("vol/nvme");
        let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
        let host = Host {
            mountpoints: vec![PathBuf::from(&vol)],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = host.mount_sources(&config, &progress, &runner);

        assert_eq!(guard.count(), 0);
        drop(guard);
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert_eq!(
            progress.logs(),
            vec![(
                LogLevel::Info,
                format!("Source volume {vol} already mounted")
            )]
        );
    }

    /// An existing but unmounted volume directory still has to be mounted —
    /// snapshotting an empty directory would "succeed" on nothing.
    #[test]
    fn source_volume_that_exists_but_is_not_mounted_gets_mounted() {
        let scratch = Scratch::new();
        let vol = scratch.mkdir("vol/nvme");
        let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
        let runner = ScriptedRunner::succeeding();

        let guard = Host::default().mount_sources(&config, &Recorder::default(), &runner);

        assert_eq!(guard.count(), 1);
        assert_eq!(
            runner.calls(),
            vec![output_call(&[
                "mount",
                "-o",
                "subvolid=5",
                "/dev/fake-nvme",
                &vol
            ])]
        );
    }

    #[test]
    fn source_whose_mount_fails_is_reported_and_not_owned_by_the_guard() {
        let scratch = Scratch::new();
        let vol = scratch.path("vol/nvme");
        let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
        let runner = ScriptedRunner::with_rules(&[(
            &vol,
            Reply::Exit {
                code: 32,
                stdout: "",
                stderr: " special device does not exist \n",
            },
        )]);
        let progress = Recorder::default();

        let guard = Host::default().mount_sources(&config, &progress, &runner);

        assert_eq!(guard.count(), 0);
        drop(guard);
        assert_eq!(runner.calls().len(), 1, "no unmount for a failed mount");
        assert_eq!(
            progress.logs(),
            vec![
                (
                    LogLevel::Warning,
                    format!(
                        "Source 'nvme': mount /dev/fake-nvme → {vol} failed: \
                         special device does not exist"
                    )
                ),
                (
                    LogLevel::Warning,
                    format!(
                        "Source 'nvme': snapshot dir {vol}/.btrbk-snapshots not created — \
                         volume '{vol}' is not mounted"
                    )
                ),
            ]
        );
    }

    #[test]
    fn source_whose_mount_cannot_be_executed_is_reported() {
        let scratch = Scratch::new();
        let vol = scratch.path("vol/nvme");
        let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
        let runner = ScriptedRunner::with_rules(&[(&vol, Reply::CannotRun)]);
        let progress = Recorder::default();

        let guard = Host::default().mount_sources(&config, &progress, &runner);

        assert_eq!(guard.count(), 0);
        let logs = progress.logs();
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert_eq!(logs[0].0, LogLevel::Warning);
        assert!(
            logs[0]
                .1
                .contains("Source 'nvme': failed to execute mount:"),
            "{logs:?}"
        );
        assert_eq!(
            logs[1],
            (
                LogLevel::Warning,
                format!(
                    "Source 'nvme': snapshot dir {vol}/.btrbk-snapshots not created — \
                     volume '{vol}' is not mounted"
                )
            )
        );
    }

    /// A volume directory that cannot be created is skipped — no mount is
    /// attempted onto a path that is not there — and the snapshot directory
    /// is not attempted either, with a line saying why.
    #[test]
    fn source_whose_volume_directory_cannot_be_created_is_skipped() {
        let scratch = Scratch::new();
        let blocker = scratch.touch("blocker");
        let vol = format!("{blocker}/nvme");
        let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = Host::default().mount_sources(&config, &progress, &runner);

        assert_eq!(guard.count(), 0);
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        let logs = progress.logs();
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert_eq!(logs[0].0, LogLevel::Warning);
        assert!(
            logs[0].1.contains("Source 'nvme': could not create") && logs[0].1.contains(&vol),
            "{logs:?}"
        );
        assert_eq!(
            logs[1],
            (
                LogLevel::Warning,
                format!(
                    "Source 'nvme': snapshot dir {vol}/.btrbk-snapshots not created — \
                     volume '{vol}' is not mounted"
                )
            )
        );
    }

    /// Inside a volume that IS mounted, a snapshot directory that cannot be
    /// created (read-only or full filesystem; here a file in the way) is
    /// reported, not swallowed (bd DAS-Backup-Manager-06p).
    #[test]
    fn snapshot_dir_that_cannot_be_created_in_a_mounted_volume_is_reported() {
        let scratch = Scratch::new();
        let vol = scratch.mkdir("vol/nvme");
        scratch.touch("vol/nvme/blocker");
        let mut src = source("nvme", "/dev/fake-nvme", &vol);
        src.snapshot_dir = "blocker/snapshots".into();
        let config = config_with(Vec::new(), vec![src]);
        let host = Host {
            mountpoints: vec![PathBuf::from(&vol)],
            ..Host::default()
        };
        let progress = Recorder::default();

        let _guard = host.mount_sources(&config, &progress, &ScriptedRunner::succeeding());

        let logs = progress.logs();
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert_eq!(logs[1].0, LogLevel::Warning);
        assert!(
            logs[1].1.contains(&format!(
                "Could not create snapshot dir {vol}/blocker/snapshots"
            )),
            "{logs:?}"
        );
    }

    /// btrbk's target subdirectories are created ONLY on a target that is
    /// really mounted. Creating them under a bare mount point is the first
    /// write of the 9on incident: it lands on the root filesystem.
    #[test]
    fn target_subdirs_are_created_only_on_mounted_targets() {
        let scratch = Scratch::new();
        let mounted = scratch.mkdir("mnt/mounted");
        let bare = scratch.mkdir("mnt/bare");
        let absent = scratch.path("mnt/absent");
        let vol = scratch.mkdir("vol/nvme");
        // Already present on the mounted target: must be accepted quietly.
        scratch.mkdir("mnt/mounted/existing");
        let mut src = source("nvme", "/dev/fake-nvme", &vol);
        src.target_subdirs = vec!["nvme/root".into(), "existing".into()];
        let config = config_with(
            vec![
                target("mounted", "S1", &mounted, TargetRole::Primary),
                target("bare", "S2", &bare, TargetRole::Mirror),
                target("absent", "S3", &absent, TargetRole::Mirror),
            ],
            vec![src],
        );
        let host = Host {
            mountpoints: vec![PathBuf::from(&vol), PathBuf::from(&mounted)],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = host.mount_sources(&config, &progress, &runner);

        assert_eq!(guard.count(), 0);
        assert!(Path::new(&mounted).join("nvme/root").is_dir());
        assert_eq!(
            std::fs::read_dir(&bare).unwrap().count(),
            0,
            "nothing may be written under a bare mount point"
        );
        assert!(!Path::new(&absent).exists());
        assert_eq!(
            progress.logs(),
            vec![(
                LogLevel::Info,
                format!("Source volume {vol} already mounted")
            )]
        );
    }

    #[test]
    fn target_subdir_that_cannot_be_created_is_reported() {
        let scratch = Scratch::new();
        let mounted = scratch.mkdir("mnt/mounted");
        scratch.touch("mnt/mounted/blocker");
        let vol = scratch.mkdir("vol/nvme");
        let mut src = source("nvme", "/dev/fake-nvme", &vol);
        src.target_subdirs = vec!["blocker/root".into()];
        let config = config_with(
            vec![target("mounted", "S1", &mounted, TargetRole::Primary)],
            vec![src],
        );
        let host = Host {
            mountpoints: vec![PathBuf::from(&vol), PathBuf::from(&mounted)],
            ..Host::default()
        };
        let progress = Recorder::default();

        let _guard = host.mount_sources(&config, &progress, &ScriptedRunner::succeeding());

        let logs = progress.logs();
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert_eq!(logs[1].0, LogLevel::Warning);
        assert!(
            logs[1].1.contains(&format!(
                "Could not create target subdir {mounted}/blocker/root"
            )),
            "{logs:?}"
        );
    }

    // -----------------------------------------------------------------------
    // No stale mount point after a failed mount
    //
    // A bare directory at a target's mount path is the artifact the
    // bare-mountpoint guard exists to catch (bd DAS-Backup-Manager-9on), and
    // `create_mount_points` in scripts/backup-run.sh rmdirs it for an
    // unavailable target. The Rust path must not manufacture one.
    // -----------------------------------------------------------------------

    #[test]
    fn target_mount_point_created_for_a_failed_mount_is_removed_again() {
        for reply in [Reply::exit(32), Reply::CannotRun] {
            let scratch = Scratch::new();
            let (config, host, _part, mnt) = attached_primary(&scratch);
            let runner = ScriptedRunner::with_rules(&[(&mnt, reply)]);
            let progress = Recorder::default();

            let result = host.mount_targets(&config, &progress, &runner);

            assert!(matches!(result, Err(MountError::NoDrivesFound)));
            assert_eq!(runner.calls().len(), 1, "the mount was attempted");
            assert!(
                !Path::new(&mnt).exists(),
                "a mount point created for a mount that failed must not be left behind"
            );
            assert_eq!(progress.logs().len(), 1, "{:?}", progress.logs());
        }
    }

    /// A directory that was there before this call is not ours to remove,
    /// even when it is empty and the mount fails.
    #[test]
    fn target_mount_point_that_already_existed_survives_a_failed_mount() {
        for reply in [Reply::exit(32), Reply::CannotRun] {
            let scratch = Scratch::new();
            let (config, host, _part, mnt) = attached_primary(&scratch);
            scratch.mkdir("mnt/p");
            let runner = ScriptedRunner::with_rules(&[(&mnt, reply)]);

            let result = host.mount_targets(&config, &Recorder::default(), &runner);

            assert!(matches!(result, Err(MountError::NoDrivesFound)));
            assert!(Path::new(&mnt).is_dir(), "pre-existing directory removed");
        }
    }

    /// `remove_dir`, never `remove_dir_all`: if the failed mount left
    /// something in the directory, it stays, and the operator is told.
    #[test]
    fn target_mount_point_that_cannot_be_removed_after_a_failed_mount_is_reported() {
        let scratch = Scratch::new();
        let (config, host, _part, mnt) = attached_primary(&scratch);
        let left = format!("{mnt}/left-behind");
        let runner = ScriptedRunner::with_rules(&[(&mnt, Reply::FailLeavingFile(left.clone()))]);
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert!(Path::new(&left).exists(), "contents must never be deleted");
        let logs = progress.logs();
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert_eq!(logs[1].0, LogLevel::Warning);
        assert!(
            logs[1].1.contains("Target 'primary'")
                && logs[1]
                    .1
                    .contains(&format!("could not remove mount point '{mnt}'")),
            "{logs:?}"
        );
    }

    fn mounted_elsewhere(scratch: &Scratch) -> (Config, Host, String) {
        let mnt = scratch.path("mnt/p");
        let config = config_with(
            vec![target("primary", "SER-P", &mnt, TargetRole::Primary)],
            Vec::new(),
        );
        let host = Host {
            mounted_elsewhere: vec![("SER-P".into(), "/run/media/u/das".into())],
            ..Host::default()
        };
        (config, host, mnt)
    }

    #[test]
    fn bind_mount_point_created_for_a_failed_bind_mount_is_removed_again() {
        for reply in [Reply::exit(1), Reply::CannotRun] {
            let scratch = Scratch::new();
            let (config, host, mnt) = mounted_elsewhere(&scratch);
            let runner = ScriptedRunner::with_rules(&[(&mnt, reply)]);
            let progress = Recorder::default();

            let result = host.mount_targets(&config, &progress, &runner);

            assert!(matches!(result, Err(MountError::NoDrivesFound)));
            assert_eq!(runner.calls().len(), 1, "the bind mount was attempted");
            assert!(
                !Path::new(&mnt).exists(),
                "a mount point created for a bind mount that failed must not be left behind"
            );
            assert_eq!(progress.logs().len(), 1, "{:?}", progress.logs());
        }
    }

    #[test]
    fn bind_mount_point_that_already_existed_survives_a_failed_bind_mount() {
        let scratch = Scratch::new();
        let (config, host, mnt) = mounted_elsewhere(&scratch);
        scratch.mkdir("mnt/p");
        let runner = ScriptedRunner::with_rules(&[(&mnt, Reply::exit(1))]);

        let result = host.mount_targets(&config, &Recorder::default(), &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert!(Path::new(&mnt).is_dir(), "pre-existing directory removed");
    }

    #[test]
    fn bind_mount_point_that_cannot_be_removed_after_a_failed_bind_mount_is_reported() {
        let scratch = Scratch::new();
        let (config, host, mnt) = mounted_elsewhere(&scratch);
        let left = format!("{mnt}/left-behind");
        let runner = ScriptedRunner::with_rules(&[(&mnt, Reply::FailLeavingFile(left.clone()))]);
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        assert!(matches!(result, Err(MountError::NoDrivesFound)));
        assert!(Path::new(&left).exists(), "contents must never be deleted");
        let logs = progress.logs();
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert_eq!(logs[1].0, LogLevel::Warning);
        assert!(
            logs[1].1.contains("Target 'primary'")
                && logs[1]
                    .1
                    .contains(&format!("could not remove mount point '{mnt}'")),
            "{logs:?}"
        );
    }

    #[test]
    fn source_volume_directory_created_for_a_failed_mount_is_removed_again() {
        for reply in [Reply::exit(32), Reply::CannotRun] {
            let scratch = Scratch::new();
            let vol = scratch.path("vol/nvme");
            let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
            let runner = ScriptedRunner::with_rules(&[(&vol, reply)]);

            let guard = Host::default().mount_sources(&config, &Recorder::default(), &runner);

            assert_eq!(guard.count(), 0);
            assert_eq!(runner.calls().len(), 1, "the mount was attempted");
            assert!(
                !Path::new(&vol).exists(),
                "a volume directory created for a mount that failed must not be left behind"
            );
        }
    }

    #[test]
    fn source_volume_directory_that_already_existed_survives_a_failed_mount() {
        for reply in [Reply::exit(32), Reply::CannotRun] {
            let scratch = Scratch::new();
            let vol = scratch.mkdir("vol/nvme");
            let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
            let runner = ScriptedRunner::with_rules(&[(&vol, reply)]);

            let guard = Host::default().mount_sources(&config, &Recorder::default(), &runner);

            assert_eq!(guard.count(), 0);
            assert!(Path::new(&vol).is_dir(), "pre-existing directory removed");
        }
    }

    #[test]
    fn source_volume_directory_that_cannot_be_removed_after_a_failed_mount_is_reported() {
        let scratch = Scratch::new();
        let vol = scratch.path("vol/nvme");
        let left = format!("{vol}/left-behind");
        let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
        let runner = ScriptedRunner::with_rules(&[(&vol, Reply::FailLeavingFile(left.clone()))]);
        let progress = Recorder::default();

        let guard = Host::default().mount_sources(&config, &progress, &runner);

        assert_eq!(guard.count(), 0);
        assert!(Path::new(&left).exists(), "contents must never be deleted");
        let logs = progress.logs();
        assert!(
            logs.iter().any(|(level, msg)| *level == LogLevel::Warning
                && msg.contains("Source 'nvme'")
                && msg.contains(&format!("could not remove mount point '{vol}'"))),
            "{logs:?}"
        );
    }

    /// Positive control for the three "removed again" tests: a mount point
    /// this call created for a mount that SUCCEEDED is in use and stays.
    #[test]
    fn mount_points_created_for_successful_mounts_are_kept() {
        let scratch = Scratch::new();
        let (config, host, _part, mnt) = attached_primary(&scratch);
        let _direct = host
            .mount_targets(&config, &Recorder::default(), &ScriptedRunner::succeeding())
            .unwrap();
        assert!(Path::new(&mnt).is_dir());

        let scratch = Scratch::new();
        let (config, host, mnt) = mounted_elsewhere(&scratch);
        let _bind = host
            .mount_targets(&config, &Recorder::default(), &ScriptedRunner::succeeding())
            .unwrap();
        assert!(Path::new(&mnt).is_dir());

        let scratch = Scratch::new();
        let vol = scratch.path("vol/nvme");
        let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
        let _source = Host::default().mount_sources(
            &config,
            &Recorder::default(),
            &ScriptedRunner::succeeding(),
        );
        assert!(Path::new(&vol).is_dir());
    }

    // -----------------------------------------------------------------------
    // Snapshot directories go inside MOUNTED volumes only
    // -----------------------------------------------------------------------

    /// After a failed mount the volume path is a bare directory on whatever
    /// filesystem holds it. A snapshot directory created there is a write to
    /// the wrong filesystem — the same class as 9on, at small scale.
    #[test]
    fn snapshot_dir_is_not_created_in_a_volume_whose_mount_failed() {
        let scratch = Scratch::new();
        let vol = scratch.mkdir("vol/nvme");
        let config = config_with(Vec::new(), vec![source("nvme", "/dev/fake-nvme", &vol)]);
        let runner = ScriptedRunner::with_rules(&[(&vol, Reply::exit(32))]);
        let progress = Recorder::default();

        let guard = Host::default().mount_sources(&config, &progress, &runner);

        assert_eq!(guard.count(), 0);
        assert_eq!(
            std::fs::read_dir(&vol).unwrap().count(),
            0,
            "nothing may be created inside an unmounted volume directory"
        );
        let logs = progress.logs();
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert_eq!(
            logs[1],
            (
                LogLevel::Warning,
                format!(
                    "Source 'nvme': snapshot dir {vol}/.btrbk-snapshots not created — \
                     volume '{vol}' is not mounted"
                )
            )
        );
    }

    /// Positive control: mounted by this call, or mounted beforehand, the
    /// snapshot directory btrbk needs IS created, without comment.
    #[test]
    fn snapshot_dir_is_created_in_a_mounted_volume() {
        let scratch = Scratch::new();
        let fresh = scratch.path("vol/fresh");
        let already = scratch.mkdir("vol/already");
        let config = config_with(
            Vec::new(),
            vec![
                source("fresh", "/dev/fake-a", &fresh),
                source("already", "/dev/fake-b", &already),
            ],
        );
        let host = Host {
            mountpoints: vec![PathBuf::from(&already)],
            ..Host::default()
        };
        let progress = Recorder::default();

        let guard = host.mount_sources(&config, &progress, &ScriptedRunner::succeeding());

        assert_eq!(guard.count(), 1);
        assert!(Path::new(&fresh).join(".btrbk-snapshots").is_dir());
        assert!(Path::new(&already).join(".btrbk-snapshots").is_dir());
        let logs = progress.logs();
        assert!(
            logs.iter().all(|(level, _)| *level == LogLevel::Info),
            "{logs:?}"
        );
    }

    /// The mount table is the authority, not the directory: a path the probe
    /// calls mounted but which is not there gets nothing created under it.
    #[test]
    fn snapshot_dir_is_not_created_under_a_volume_path_that_does_not_exist() {
        let scratch = Scratch::new();
        let vol = scratch.path("vol/gone");
        let config = config_with(Vec::new(), vec![source("gone", "/dev/fake", &vol)]);
        // Failing the mount removes the directory this call created; the
        // described host still (wrongly) lists the path as a mount point.
        let host = Host {
            mountpoints: vec![PathBuf::from(&vol)],
            ..Host::default()
        };
        let runner = ScriptedRunner::with_rules(&[(&vol, Reply::exit(32))]);
        let progress = Recorder::default();

        let _guard = host.mount_sources(&config, &progress, &runner);

        assert!(!Path::new(&vol).exists(), "{vol} must not be re-created");
        assert!(
            progress
                .logs()
                .iter()
                .any(|(level, msg)| *level == LogLevel::Warning
                    && msg.contains("Source 'gone': snapshot dir")
                    && msg.contains("is not mounted")),
            "{:?}",
            progress.logs()
        );
    }

    /// An empty or relative `volume` resolves against the process's working
    /// directory. That is how empty root-owned `.btrbk-snapshots` directories
    /// came to exist inside the source tree: nothing may be created for one,
    /// whatever the mount table is made to say.
    #[test]
    fn snapshot_dir_is_never_created_for_an_empty_or_relative_volume() {
        // `src` exists relative to the crate root, where cargo runs tests.
        assert!(
            Path::new("src").is_dir(),
            "test must run from the crate root"
        );
        for (volume, would_create) in [
            ("", "das-mount-test-snapdir-empty"),
            ("src", "src/das-mount-test-snapdir-relative"),
        ] {
            let snapshot_dir = Path::new(would_create).file_name().unwrap();
            let mut src = source("stray", "/dev/fake", volume);
            src.snapshot_dir = snapshot_dir.to_string_lossy().into_owned();
            let config = config_with(Vec::new(), vec![src]);
            let host = Host {
                mountpoints: vec![PathBuf::from(volume)],
                ..Host::default()
            };
            let runner = ScriptedRunner::with_rules(&[(volume, Reply::exit(32))]);
            let progress = Recorder::default();

            let _guard = host.mount_sources(&config, &progress, &runner);

            // Look, then clean up, then judge — a failure must not leave the
            // stray directory behind in the source tree.
            let created = Path::new(would_create).exists();
            if created {
                std::fs::remove_dir(would_create).unwrap();
            }
            assert!(
                !created,
                "volume {volume:?}: created {would_create} relative to the working directory"
            );
            assert!(
                progress
                    .logs()
                    .iter()
                    .any(|(level, msg)| *level == LogLevel::Warning
                        && msg.contains("Source 'stray': snapshot dir")
                        && msg.contains("is not mounted")),
                "volume {volume:?}: {:?}",
                progress.logs()
            );
        }
    }

    // -----------------------------------------------------------------------
    // filesystem_uuid_at
    // -----------------------------------------------------------------------

    // --- create_target_dirs (bd DAS-Backup-Manager-arx review) -------------

    fn dirs_config(scratch: &Scratch, subdirs: Vec<String>) -> Config {
        let mut src = source("nvme-adopted", "UUID=abc", "/.btrfs-nvme");
        src.target_subdirs = subdirs;
        src.subvolumes = vec![crate::config::SubvolConfig {
            name: "@new".into(),
            ..Default::default()
        }];
        config_with(
            vec![target_at("t", &scratch.mkdir("mnt/t"), Some("u"))],
            vec![src],
        )
    }

    fn create_dirs(
        host: &Host,
        config: &Config,
        labels: &[&str],
        progress: &Recorder,
    ) -> Result<(), String> {
        let runner = ScriptedRunner::succeeding();
        let labels: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
        host.with_probes(&runner, |p| {
            create_target_dirs_with(config, &labels, progress, p)
        })
    }

    #[test]
    fn a_freshly_mounted_target_gets_the_directory_it_receives_into() {
        let scratch = Scratch::new();
        let config = dirs_config(&scratch, vec!["nvme-adopted".into(), "second".into()]);
        let host = Host {
            mountpoints: vec![PathBuf::from(scratch.path("mnt/t"))],
            ..Host::default()
        };
        let progress = Recorder::default();
        create_dirs(&host, &config, &["t"], &progress).unwrap();
        assert!(Path::new(&scratch.path("mnt/t/nvme-adopted")).is_dir());
        assert!(
            !Path::new(&scratch.path("mnt/t/second")).exists(),
            "only the subdirectory the renderer uses"
        );
        progress.assert_only_log(LogLevel::Info, &["Target 't': created", "nvme-adopted"]);

        // Already there: nothing logged.
        let again = Recorder::default();
        create_dirs(&host, &config, &["t"], &again).unwrap();
        assert!(again.logs().is_empty());
    }

    #[test]
    fn nothing_is_created_under_a_bare_directory_and_the_run_is_told() {
        let scratch = Scratch::new();
        let config = dirs_config(&scratch, vec!["nvme-adopted".into()]);
        // mnt/t exists but is NOT a mount point: the bare-mountpoint trap.
        let err = create_dirs(&Host::default(), &config, &["t"], &Recorder::default()).unwrap_err();
        assert!(err.contains("is not a mount point"), "{err}");
        assert!(!Path::new(&scratch.path("mnt/t/nvme-adopted")).exists());
    }

    #[test]
    fn a_target_not_written_this_run_is_left_alone() {
        let scratch = Scratch::new();
        let config = dirs_config(&scratch, vec!["nvme-adopted".into()]);
        create_dirs(&Host::default(), &config, &[], &Recorder::default()).unwrap();
        assert!(!Path::new(&scratch.path("mnt/t/nvme-adopted")).exists());
    }

    #[test]
    fn a_subdirectory_that_is_absolute_or_climbs_is_refused() {
        let scratch = Scratch::new();
        let host = Host {
            mountpoints: vec![PathBuf::from(scratch.path("mnt/t"))],
            ..Host::default()
        };
        for bad in ["../escape", "/abs", "a/../../b", "./x"] {
            let config = dirs_config(&scratch, vec![bad.to_string()]);
            let err = create_dirs(&host, &config, &["t"], &Recorder::default()).unwrap_err();
            assert!(err.contains("not a plain relative path"), "{bad}: {err}");
        }
        assert!(!Path::new(&scratch.path("mnt/escape")).exists());
        // A nested plain path is fine.
        let config = dirs_config(&scratch, vec!["a/b".into()]);
        create_dirs(&host, &config, &["t"], &Recorder::default()).unwrap();
        assert!(Path::new(&scratch.path("mnt/t/a/b")).is_dir());
    }

    #[test]
    fn a_directory_that_cannot_be_created_fails_loudly() {
        let scratch = Scratch::new();
        let config = dirs_config(&scratch, vec!["nvme-adopted".into()]);
        // A file where the directory belongs.
        std::fs::write(scratch.path("mnt/t/nvme-adopted"), b"x").unwrap();
        let host = Host {
            mountpoints: vec![PathBuf::from(scratch.path("mnt/t"))],
            ..Host::default()
        };
        let err = create_dirs(&host, &config, &["t"], &Recorder::default()).unwrap_err();
        assert!(err.starts_with("Target directories missing"), "{err}");
        assert!(err.contains("t: could not create"), "{err}");
    }

    #[test]
    fn filesystem_uuid_is_read_from_findmnt_and_absent_on_any_failure() {
        let uuid = "5eed0016-0000-4000-8000-000000000016";
        let found: &'static str = "5eed0016-0000-4000-8000-000000000016\n";
        let cases: [(&str, Reply, Option<&str>); 5] = [
            (
                "a UUID",
                Reply::Exit {
                    code: 0,
                    stdout: found,
                    stderr: "",
                },
                Some(uuid),
            ),
            ("nothing printed", Reply::ok(), None),
            (
                "only whitespace printed",
                Reply::Exit {
                    code: 0,
                    stdout: "  \n",
                    stderr: "",
                },
                None,
            ),
            (
                // Output from a findmnt that says it FAILED is not an answer.
                "a UUID printed by a failing findmnt",
                Reply::Exit {
                    code: 1,
                    stdout: found,
                    stderr: "",
                },
                None,
            ),
            ("findmnt unavailable", Reply::CannotRun, None),
        ];

        for (name, reply, expected) in cases {
            let runner = ScriptedRunner::with_rules(&[("/mnt/x", reply)]);

            let got = filesystem_uuid_with(&*runner, "/mnt/x");

            assert_eq!(got.as_deref(), expected, "{name}");
            assert_eq!(
                runner.calls(),
                vec![output_call(&[
                    "findmnt", "-n", "-o", "UUID", "--target", "/mnt/x"
                ])],
                "{name}"
            );
        }
    }

    /// The public entry point must reach the real `findmnt`. Compared against
    /// an independent invocation rather than a fixed value, because which
    /// filesystems exist is the test machine's business.
    #[test]
    fn filesystem_uuid_at_agrees_with_findmnt_on_this_machine() {
        let listing = Command::new("findmnt")
            .args(["-n", "-l", "-o", "UUID,TARGET"])
            .output()
            .expect("findmnt (util-linux) is required to run this test");
        assert!(listing.status.success());
        let listing = String::from_utf8_lossy(&listing.stdout).into_owned();

        // A mount whose filesystem has a UUID: a real answer is expected.
        let with_uuid = listing.lines().find_map(|line| {
            let mut cols = line.split_whitespace();
            match (cols.next(), cols.next(), cols.next()) {
                (Some(uuid), Some(target), None) if target.starts_with('/') => {
                    Some((uuid.to_string(), target.to_string()))
                }
                _ => None,
            }
        });
        // A container has none (every mount is an overlay or a virtual
        // filesystem), so there the positive half cannot be asked. The
        // mutation job runs on a real machine for exactly this reason: where
        // this half is skipped, `filesystem_uuid_at -> None` survives, and the
        // gate says so.
        match with_uuid {
            Some((_, target)) => {
                let direct = Command::new("findmnt")
                    .args(["-n", "-o", "UUID", "--target", &target])
                    .output()
                    .unwrap();
                let direct = String::from_utf8_lossy(&direct.stdout).trim().to_string();
                assert!(!direct.is_empty(), "{target}");
                assert_eq!(filesystem_uuid_at(&target), Some(direct), "{target}");
            }
            None => eprintln!(
                "SKIP filesystem_uuid_at positive case: no mounted filesystem \
                 with a UUID on this machine"
            ),
        }

        // procfs has no UUID: no answer, not an empty or invented one.
        assert_eq!(filesystem_uuid_at("/proc"), None);
    }

    // -----------------------------------------------------------------------
    // verify_write_targets
    // -----------------------------------------------------------------------

    const EXPECTED_UUID: &str = "5eed0016-0000-4000-8000-000000000016";
    const EXPECTED_UUID_LINE: &str = "5eed0016-0000-4000-8000-000000000016\n";

    fn mounted_write_target(scratch: &Scratch, uuid: Option<&str>) -> (Vec<Target>, Host, String) {
        let mnt = scratch.mkdir("mnt/p");
        let host = Host {
            mountpoints: vec![PathBuf::from(&mnt)],
            ..Host::default()
        };
        (vec![target_at("primary-22tb", &mnt, uuid)], host, mnt)
    }

    #[test]
    fn verify_accepts_a_write_target_carrying_the_expected_filesystem() {
        let scratch = Scratch::new();
        let (targets, host, mnt) = mounted_write_target(&scratch, Some(EXPECTED_UUID));
        let runner = ScriptedRunner::with_rules(&[(
            &mnt,
            Reply::Exit {
                code: 0,
                stdout: EXPECTED_UUID_LINE,
                stderr: "",
            },
        )]);
        let progress = Recorder::default();

        let result = host.verify(&targets, &["primary-22tb"], &progress, &runner);

        assert_eq!(result, Ok(()));
        assert_eq!(
            runner.calls(),
            vec![output_call(&[
                "findmnt", "-n", "-o", "UUID", "--target", &mnt
            ])]
        );
        assert_eq!(
            progress.logs(),
            vec![(
                LogLevel::Info,
                format!("  primary-22tb: OK ({mnt} → UUID={EXPECTED_UUID})")
            )]
        );
    }

    /// The 2026-11-21 shape, whoever mounted it: a RAID-1 target holding the
    /// right filesystem, both drives attached, and the kernel reporting
    /// devid 2 missing — refused, never OK (bd DAS-Backup-Manager-su1b.10).
    /// With a drive truly absent it is a degraded mount, said, not refused.
    #[test]
    fn verify_refuses_a_raid1_target_missing_a_member_while_both_drives_are_attached() {
        let scratch = Scratch::new();
        let (mut targets, mut host, mnt) = mounted_write_target(&scratch, Some(EXPECTED_UUID));
        targets[0].serials = vec!["SERIAL11".into(), "SERIAL12".into()];
        host.devices = vec![
            ("SERIAL11".into(), "/dev/sdb".into()),
            ("SERIAL12".into(), "/dev/sdc".into()),
        ];
        host.missing = vec![(EXPECTED_UUID.into(), vec![2])];
        let runner = || {
            ScriptedRunner::with_rules(&[(
                &mnt,
                Reply::Exit {
                    code: 0,
                    stdout: EXPECTED_UUID_LINE,
                    stderr: "",
                },
            )])
        };

        let err = host
            .verify(&targets, &["primary-22tb"], &Recorder::default(), &runner())
            .unwrap_err();
        assert!(
            err.contains("[2] missing while every configured drive"),
            "{err}"
        );

        host.devices.pop();
        let progress = Recorder::default();
        assert_eq!(
            host.verify(&targets, &["primary-22tb"], &progress, &runner()),
            Ok(())
        );
        assert!(
            progress
                .logs()
                .iter()
                .any(|(l, m)| *l == LogLevel::Warning && m.contains("RAID-1 DEGRADED")),
            "{:?}",
            progress.logs()
        );

        host.devices.push(("SERIAL12".into(), "/dev/sdc".into()));
        host.missing.clear();
        assert_eq!(
            host.verify(&targets, &["primary-22tb"], &Recorder::default(), &runner()),
            Ok(()),
            "nothing missing"
        );
    }

    /// The real test of the UUID check: something IS mounted, findmnt DOES
    /// answer, and the answer is a different filesystem.
    #[test]
    fn verify_refuses_a_write_target_carrying_a_different_filesystem() {
        let scratch = Scratch::new();
        let (targets, host, mnt) = mounted_write_target(&scratch, Some(EXPECTED_UUID));
        let runner = ScriptedRunner::with_rules(&[(
            &mnt,
            Reply::Exit {
                code: 0,
                stdout: "5eed0008-0000-0000-0000-000000000000\n",
                stderr: "",
            },
        )]);
        let progress = Recorder::default();

        let err = host
            .verify(&targets, &["primary-22tb"], &progress, &runner)
            .unwrap_err();

        assert!(err.starts_with("Refusing to run btrbk"), "{err}");
        assert!(
            err.contains(&format!(
                "primary-22tb: '{mnt}' has filesystem UUID \
                 '5eed0008-0000-0000-0000-000000000000', expected '{EXPECTED_UUID}'"
            )),
            "{err}"
        );
        assert!(progress.logs().is_empty(), "{:?}", progress.logs());
    }

    /// "Could not find out" is a refusal, not a pass.
    #[test]
    fn verify_refuses_a_write_target_whose_filesystem_cannot_be_identified() {
        for reply in [Reply::ok(), Reply::exit(1), Reply::CannotRun] {
            let scratch = Scratch::new();
            let (targets, host, mnt) = mounted_write_target(&scratch, Some(EXPECTED_UUID));
            let runner = ScriptedRunner::with_rules(&[(&mnt, reply)]);

            let err = host
                .verify(&targets, &["primary-22tb"], &Recorder::default(), &runner)
                .unwrap_err();

            assert!(
                err.contains("could not determine the filesystem UUID") && err.contains(&mnt),
                "{err}"
            );
        }
    }

    // --- a target with no mount_uuid is checked by its drive's serial ----------
    //
    // bd DAS-Backup-Manager-7tx 4: the Rust path checked a legacy target only
    // as "is a mount point"; backup-run.sh's verify_targets_before_btrbk also
    // finds the serial of the drive under it. The three questions are
    // findmnt (which device), lsblk (which disk) and smartctl (which serial);
    // each is keyed here by an argument only it has.

    const SMART_INFO: &str = "smartctl 7.5 2025-04-30 r5714 [x86_64-linux-7.2.8] (local build)\n\
        === START OF INFORMATION SECTION ===\n\
        Device Model:     ST22000NM000C-3WC103\n\
        Serial Number:    SERIAL12\n\
        Firmware Version: SN03\n";

    fn serial_target(scratch: &Scratch, serials: &[&str]) -> (Vec<Target>, Host, String) {
        let (mut targets, host, mnt) = mounted_write_target(scratch, None);
        targets[0].serials = serials.iter().map(|s| s.to_string()).collect();
        targets[0].serial = serials.first().map_or_else(String::new, |s| s.to_string());
        (targets, host, mnt)
    }

    fn drive_reply(stdout: &'static str) -> Reply {
        Reply::Exit {
            code: 0,
            stdout,
            stderr: "",
        }
    }

    /// A runner answering the three questions: where the mount is from, what
    /// disk that is on, and what that disk's serial is.
    fn drive_runner(mnt: &str, source: Reply, disk: Reply, smart: Reply) -> Arc<ScriptedRunner> {
        ScriptedRunner::with_rules(&[(mnt, source), ("PKNAME", disk), ("-i", smart)])
    }

    #[test]
    fn a_legacy_target_is_accepted_when_the_drive_under_it_has_a_configured_serial() {
        for uuid in [None, Some("")] {
            let scratch = Scratch::new();
            let (mut targets, host, mnt) = serial_target(&scratch, &["SERIAL12"]);
            targets[0].mount_uuid = uuid.map(str::to_string);
            // findmnt appends the btrfs subvolume to the device.
            let runner = drive_runner(
                &mnt,
                drive_reply("/dev/sdj1[/@backups]\n"),
                drive_reply("sdj\n"),
                drive_reply(SMART_INFO),
            );
            let progress = Recorder::default();

            let result = host.verify(&targets, &["primary-22tb"], &progress, &runner);

            assert_eq!(result, Ok(()), "{uuid:?}");
            assert_eq!(
                runner.calls(),
                vec![
                    output_call(&["findmnt", "-n", "-o", "SOURCE", "--target", &mnt]),
                    output_call(&["lsblk", "-dno", "PKNAME", "/dev/sdj1"]),
                    output_call(&["smartctl", "-i", "/dev/sdj"]),
                ]
            );
            assert_eq!(
                progress.logs(),
                vec![(
                    LogLevel::Info,
                    format!("  primary-22tb: OK ({mnt} → /dev/sdj1, serial=SERIAL12)")
                )]
            );
        }
    }

    /// A RAID-1 target names two drives, and either is the one mounted.
    #[test]
    fn either_serial_of_a_raid_pair_is_enough_and_the_other_is_not_needed() {
        let scratch = Scratch::new();
        let (targets, host, mnt) = serial_target(&scratch, &["SERIAL11", "SERIAL12"]);
        let runner = drive_runner(
            &mnt,
            drive_reply("/dev/sdj1\n"),
            drive_reply("sdj\n"),
            drive_reply(SMART_INFO),
        );
        assert_eq!(
            host.verify(&targets, &["primary-22tb"], &Recorder::default(), &runner),
            Ok(())
        );
    }

    /// A whole disk has no parent: it is its own disk.
    #[test]
    fn a_device_with_no_parent_disk_is_its_own_disk() {
        let scratch = Scratch::new();
        let (targets, host, mnt) = serial_target(&scratch, &["SERIAL12"]);
        let runner = drive_runner(
            &mnt,
            drive_reply("/dev/sdj\n"),
            drive_reply("\n"),
            drive_reply(SMART_INFO),
        );
        assert_eq!(
            host.verify(&targets, &["primary-22tb"], &Recorder::default(), &runner),
            Ok(())
        );
        assert_eq!(
            runner.calls()[2],
            output_call(&["smartctl", "-i", "/dev/sdj"])
        );
    }

    /// The counter-test: another drive is under the mount point.
    #[test]
    fn a_legacy_target_whose_drive_has_another_serial_is_refused_and_says_which() {
        let scratch = Scratch::new();
        let (targets, host, mnt) = serial_target(&scratch, &["SERIAL11", "SERIAL04"]);
        let runner = drive_runner(
            &mnt,
            drive_reply("/dev/sdj1\n"),
            drive_reply("sdj\n"),
            drive_reply(SMART_INFO),
        );
        let progress = Recorder::default();

        let err = host
            .verify(&targets, &["primary-22tb"], &progress, &runner)
            .unwrap_err();

        assert!(err.starts_with("Refusing to run btrbk"), "{err}");
        assert!(
            err.contains(&format!(
                "primary-22tb: '{mnt}' mounted from /dev/sdj1 (serial='SERIAL12'), expected one \
                 of: SERIAL11 SERIAL04"
            )),
            "{err}"
        );
        assert!(progress.logs().is_empty(), "{:?}", progress.logs());
    }

    /// Nothing that cannot be read is a pass: each question can fail, and each
    /// failure is a refusal.
    #[test]
    fn a_serial_that_cannot_be_read_is_a_refusal_never_a_pass() {
        let good_source = || drive_reply("/dev/sdj1\n");
        let good_disk = || drive_reply("sdj\n");
        let good_smart = || drive_reply(SMART_INFO);
        // (what findmnt, lsblk and smartctl answer, what the refusal must say)
        let cases: Vec<(&str, Reply, Reply, Reply, &str)> = vec![
            (
                "findmnt cannot run",
                Reply::CannotRun,
                good_disk(),
                good_smart(),
                "could not resolve its source device",
            ),
            (
                "findmnt fails",
                Reply::exit(1),
                good_disk(),
                good_smart(),
                "could not resolve its source device",
            ),
            (
                "findmnt says nothing",
                drive_reply(""),
                good_disk(),
                good_smart(),
                "could not resolve its source device",
            ),
            (
                "findmnt names no device",
                drive_reply("proc\n"),
                good_disk(),
                good_smart(),
                "could not resolve its source device",
            ),
            (
                "lsblk cannot run",
                good_source(),
                Reply::CannotRun,
                good_smart(),
                "serial='?'",
            ),
            (
                "lsblk fails",
                good_source(),
                Reply::exit(32),
                good_smart(),
                "serial='?'",
            ),
            (
                "smartctl cannot run",
                good_source(),
                good_disk(),
                Reply::CannotRun,
                "serial='?'",
            ),
            (
                "smartctl prints no serial",
                good_source(),
                good_disk(),
                drive_reply("Device Model: X\n"),
                "serial='?'",
            ),
            (
                "smartctl prints an empty serial",
                good_source(),
                good_disk(),
                drive_reply("Serial Number:   \n"),
                "serial='?'",
            ),
            (
                "smartctl fails without a serial",
                good_source(),
                good_disk(),
                Reply::exit(2),
                "serial='?'",
            ),
        ];
        for (what, source, disk, smart, says) in cases {
            let scratch = Scratch::new();
            let (targets, host, mnt) = serial_target(&scratch, &["SERIAL12"]);
            let runner = drive_runner(&mnt, source, disk, smart);

            let err = host
                .verify(&targets, &["primary-22tb"], &Recorder::default(), &runner)
                .unwrap_err();

            assert!(err.starts_with("Refusing to run btrbk"), "{what}: {err}");
            assert!(err.contains(says), "{what}: {err}");
            assert!(err.contains(&mnt), "{what}: {err}");
        }
    }

    /// smartctl's exit status is a bitmask of what it found (a failing disk, a
    /// logged error), not of whether it could read the drive: a serial that was
    /// printed is a serial.
    #[test]
    fn a_serial_smartctl_printed_counts_whatever_its_exit_status() {
        let scratch = Scratch::new();
        let (targets, host, mnt) = serial_target(&scratch, &["SERIAL12"]);
        let runner = drive_runner(
            &mnt,
            drive_reply("/dev/sdj1\n"),
            drive_reply("sdj\n"),
            Reply::Exit {
                code: 4,
                stdout: SMART_INFO,
                stderr: "",
            },
        );
        assert_eq!(
            host.verify(&targets, &["primary-22tb"], &Recorder::default(), &runner),
            Ok(())
        );
    }

    #[test]
    fn a_legacy_target_with_no_serial_at_all_is_refused_without_asking_anything() {
        let scratch = Scratch::new();
        let (targets, host, mnt) = serial_target(&scratch, &[]);
        let runner = ScriptedRunner::succeeding();

        let err = host
            .verify(&targets, &["primary-22tb"], &Recorder::default(), &runner)
            .unwrap_err();

        assert!(
            err.contains("has no mount_uuid and no serial in the config") && err.contains(&mnt),
            "{err}"
        );
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    /// A target with a `mount_uuid` is checked by it alone: the serial
    /// commands are never run.
    #[test]
    fn a_target_with_a_uuid_is_not_asked_for_its_serial() {
        let scratch = Scratch::new();
        let (targets, host, mnt) = mounted_write_target(&scratch, Some(EXPECTED_UUID));
        let runner = ScriptedRunner::with_rules(&[(&mnt, drive_reply(EXPECTED_UUID_LINE))]);
        assert_eq!(
            host.verify(&targets, &["primary-22tb"], &Recorder::default(), &runner),
            Ok(())
        );
        assert!(
            runner
                .calls()
                .iter()
                .all(|c| c.argv[0] == "findmnt" && c.argv.contains(&"UUID".to_string())),
            "{:?}",
            runner.calls()
        );
    }

    /// The real commands, against a real mount point that is no configured
    /// drive: `/proc` is `proc`, not a device, so it is refused (and a
    /// machine with no `findmnt` refuses it the same way).
    #[test]
    fn a_real_mount_point_that_is_no_configured_drive_is_refused() {
        let targets = vec![target_at("primary-22tb", "/proc", None)];
        let progress = TestProgress::new();

        let err =
            verify_write_targets(&targets, &["primary-22tb".to_string()], &progress).unwrap_err();

        assert!(
            err.contains("primary-22tb: '/proc' is a mount point but findmnt could not resolve its source device"),
            "{err}"
        );
    }

    /// Every failing write target is named in the one refusal.
    #[test]
    fn verify_lists_every_violation() {
        let scratch = Scratch::new();
        let bare = scratch.mkdir("mnt/bare");
        let wrong = scratch.mkdir("mnt/wrong");
        let targets = vec![
            target_at("bare", &bare, Some(EXPECTED_UUID)),
            target_at("wrong", &wrong, Some(EXPECTED_UUID)),
        ];
        let host = Host {
            mountpoints: vec![PathBuf::from(&wrong)],
            ..Host::default()
        };
        let runner = ScriptedRunner::with_rules(&[(
            &wrong,
            Reply::Exit {
                code: 0,
                stdout: "other-uuid\n",
                stderr: "",
            },
        )]);

        let err = host
            .verify(&targets, &["bare", "wrong"], &Recorder::default(), &runner)
            .unwrap_err();

        assert!(err.contains("bare (Primary): expected mounted"), "{err}");
        assert!(err.contains("NOT a mount point"), "{err}");
        assert!(err.contains("has filesystem UUID 'other-uuid'"), "{err}");
        assert_eq!(
            runner.calls().len(),
            1,
            "a bare directory has no filesystem to ask about"
        );
    }

    /// A non-write target that is a bare, NON-EMPTY directory is how 9on
    /// looked the morning after: worth a warning, not worth aborting.
    #[test]
    fn verify_warns_about_a_non_empty_bare_non_write_target() {
        let scratch = Scratch::new();
        let bare = scratch.mkdir("mnt/bare");
        scratch.touch("mnt/bare/leftover");
        let targets = vec![target_at("not-in-this-run", &bare, None)];
        let progress = Recorder::default();

        let result =
            Host::default().verify(&targets, &[], &progress, &ScriptedRunner::succeeding());

        assert_eq!(result, Ok(()));
        progress.assert_only_log(
            LogLevel::Warning,
            &[
                "Target 'not-in-this-run'",
                &bare,
                "non-empty bare directory",
            ],
        );
    }

    /// Positive control: the same non-empty directory, MOUNTED, is a healthy
    /// target full of backups and must not be described as a leak.
    #[test]
    fn verify_stays_quiet_about_a_mounted_non_write_target() {
        let scratch = Scratch::new();
        let mnt = scratch.mkdir("mnt/p");
        scratch.touch("mnt/p/snapshot");
        let targets = vec![target_at("not-in-this-run", &mnt, Some(EXPECTED_UUID))];
        let host = Host {
            mountpoints: vec![PathBuf::from(&mnt)],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let result = host.verify(&targets, &[], &progress, &runner);

        assert_eq!(result, Ok(()));
        assert!(progress.logs().is_empty(), "{:?}", progress.logs());
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    fn target_at(label: &str, mount: &str, uuid: Option<&str>) -> Target {
        Target {
            label: label.into(),
            serial: "TESTSERIAL".into(),
            serials: vec!["TESTSERIAL".into()],
            mount_uuid: uuid.map(|u| u.to_string()),
            mount: mount.into(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 7,
                yearly: 1,
            },
            display_name: label.into(),
            esp_partuuid: None,
        }
    }

    /// A write target that is NOT a mount point must abort the run.
    ///
    /// Counter-test for bd DAS-Backup-Manager-aea / 9on: handing btrbk a bare
    /// directory makes it write through to the underlying filesystem — the
    /// NVMe root — and fill it.
    #[test]
    fn verify_refuses_a_write_target_that_is_not_a_mountpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let bare = dir.path().to_string_lossy().into_owned();
        let targets = vec![target_at("primary-22tb", &bare, None)];
        let progress = TestProgress::new();

        let err =
            verify_write_targets(&targets, &["primary-22tb".to_string()], &progress).unwrap_err();

        assert!(err.contains("NOT a mount point"), "{err}");
        assert!(err.contains("primary-22tb"), "{err}");
    }

    /// A mount point carrying the WRONG filesystem must abort: the point of the
    /// UUID check is that "something is mounted here" is not the question.
    #[test]
    fn verify_refuses_a_mountpoint_with_an_unexpected_filesystem_uuid() {
        let targets = vec![target_at(
            "primary-22tb",
            "/proc",
            Some("00000000-0000-0000-0000-000000000000"),
        )];
        let progress = TestProgress::new();

        let err =
            verify_write_targets(&targets, &["primary-22tb".to_string()], &progress).unwrap_err();

        assert!(
            err.contains("UUID") || err.contains("could not determine"),
            "{err}"
        );
    }

    /// bd su1b.16: a different filesystem is refused for a RAID-1 target too,
    /// not waved through because the target is guarded.
    #[test]
    fn verify_refuses_a_different_filesystem_on_a_raid1_target() {
        let scratch = Scratch::new();
        let (mut targets, mut host, mnt) = mounted_write_target(&scratch, Some(EXPECTED_UUID));
        targets[0].serials = vec!["SERIAL11".into(), "SERIAL12".into()];
        host.devices = vec![
            ("SERIAL11".into(), "/dev/sdb".into()),
            ("SERIAL12".into(), "/dev/sdc".into()),
        ];
        let runner = ScriptedRunner::with_rules(&[(
            &mnt,
            Reply::Exit {
                code: 0,
                stdout: "5eed0008-0000-0000-0000-000000000000\n",
                stderr: "",
            },
        )]);
        let err = host
            .verify(&targets, &["primary-22tb"], &Recorder::default(), &runner)
            .unwrap_err();
        assert!(
            err.contains("a different filesystem is mounted here"),
            "{err}"
        );
    }

    /// bd su1b.16: the guarded mount counts as the first of one target.
    #[test]
    fn a_guarded_mount_reports_its_step_as_the_first_of_one() {
        let scratch = Scratch::new();
        let (mut config, mut host, part_b, mnt) = attached_primary(&scratch);
        let part_c = scratch.touch("dev/sdc1");
        config.targets[0].mount_uuid = Some("u-p".into());
        config.targets[0].serials = vec!["SER-P".into(), "SER-Q".into()];
        host.devices.push(("SER-Q".into(), scratch.path("dev/sdc")));
        let runner = ScriptedRunner::with_rules(&[
            ("blkid", Reply::Out(0, format!("{part_b}\n{part_c}\n"))),
            (
                &part_b,
                Reply::Out(
                    0,
                    super_of("u-p", 1, "5eed00a1-0000-4000-8000-0000000000a1", 40),
                ),
            ),
            (
                &part_c,
                Reply::Out(
                    0,
                    super_of("u-p", 2, "5eed00a2-0000-4000-8000-0000000000a2", 40),
                ),
            ),
        ]);
        let progress = Recorder::default();
        let guard = host.mount_targets(&config, &progress, &runner).unwrap();
        assert_eq!(guard.count(), 1);
        let steps = progress.steps();
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!((steps[0].0, steps[0].1), (1, 1), "{steps:?}");
        assert!(steps[0].2.starts_with(&format!("Mounted primary at {mnt}")));
    }

    fn system_members<R>(
        host: &Host,
        runner: &ScriptedRunner,
        f: impl FnOnce(&SystemMembers<'_>) -> R,
    ) -> R {
        host.with_probes(runner, |probes| f(&SystemMembers { runner, probes }))
    }

    /// bd su1b.16: the production host's answers, from a scripted runner.
    #[test]
    fn the_system_host_reads_members_from_blkid_and_says_what_failed() {
        use raid_guard::MemberHost;
        let host = Host::default();
        let listed = ScriptedRunner::with_rules(&[(
            "blkid",
            Reply::Out(0, " /dev/a1\n\n/dev/b1 \n".into()),
        )]);
        assert_eq!(
            system_members(&host, &listed, |m| m.members("fs")),
            Ok(vec!["/dev/a1".to_string(), "/dev/b1".to_string()])
        );
        // blkid's "nothing matched" is no member, not an error.
        let none = ScriptedRunner::with_rules(&[("blkid", Reply::exit(2))]);
        assert_eq!(
            system_members(&host, &none, |m| m.members("fs")),
            Ok(Vec::new())
        );
        let broken = ScriptedRunner::with_rules(&[(
            "blkid",
            Reply::Exit {
                code: 4,
                stdout: "",
                stderr: "boom\n",
            },
        )]);
        assert_eq!(
            system_members(&host, &broken, |m| m.members("fs")),
            Err("blkid exited with code 4: boom".to_string())
        );
    }

    #[test]
    fn a_failed_command_is_described_by_its_code_and_its_stderr() {
        let out = |code: i32, stderr: &str| Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert_eq!(failed("mount", &out(32, "")), "mount exited with code 32");
        assert_eq!(
            failed("mount", &out(32, "  wrong fs type \n")),
            "mount exited with code 32: wrong fs type"
        );
    }

    #[test]
    fn the_system_host_answers_serial_umount_missing_pause_and_clock_for_real() {
        use raid_guard::MemberHost;
        let host = Host {
            devices: vec![("SER-A".into(), "/dev/sdx".into())],
            missing: vec![("fs".into(), vec![2])],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        system_members(&host, &runner, |m| {
            assert!(m.serial_attached("SER-A"));
            assert!(!m.serial_attached("SER-Z"));
            assert_eq!(m.missing_devids("fs"), Ok(vec![2]));
            assert_eq!(m.umount("/mnt/x"), Ok(()));
            m.pause(Duration::from_secs(7));
            let before = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let now = m.now();
            assert!(now >= before && now <= before + 5, "{now} vs {before}");
        });
        assert_eq!(*runner.pauses.lock().unwrap(), vec![Duration::from_secs(7)]);
        assert!(runner.calls().contains(&status_call(&["umount", "/mnt/x"])));
        // A mount that will not come down is an error.
        let stuck = ScriptedRunner::with_rules(&[("umount", Reply::exit(32))]);
        let err = system_members(&host, &stuck, |m| m.umount("/mnt/x")).unwrap_err();
        assert!(err.contains("umount exited with code 32"), "{err}");
    }

    /// A target that is not part of this run is never fatal — btrbk is not
    /// being told to write there.
    #[test]
    fn verify_ignores_targets_that_are_not_write_targets() {
        let dir = tempfile::TempDir::new().unwrap();
        let bare = dir.path().to_string_lossy().into_owned();
        let targets = vec![target_at("not-in-this-run", &bare, None)];
        let progress = TestProgress::new();

        assert!(verify_write_targets(&targets, &[], &progress).is_ok());
    }

    /// A non-write target whose mount path exists but cannot be LISTED must
    /// say so. `read_dir(..).unwrap_or(false)` read "could not look" as
    /// "empty — nothing to see", so a plain file sitting where a mount point
    /// belongs (read_dir fails ENOTDIR) produced no log line at all — the
    /// silence being indistinguishable from a clean, empty directory.
    #[test]
    fn verify_reports_a_non_write_target_it_cannot_list() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_at_mount = dir.path().join("mount-point-is-a-file");
        std::fs::write(&file_at_mount, b"x").unwrap();
        let path = file_at_mount.to_string_lossy().into_owned();
        let targets = vec![target_at("not-in-this-run", &path, None)];
        let progress = TestProgress::new();

        // Still not fatal — it is not a write target this run.
        assert!(verify_write_targets(&targets, &[], &progress).is_ok());

        let logs = progress.logs.lock().unwrap();
        let warned = logs.iter().any(|(lvl, msg)| {
            *lvl == crate::progress::LogLevel::Warning
                && msg.contains("could not be listed")
                && msg.contains("not-in-this-run")
        });
        assert!(
            warned,
            "an unlistable target path must be reported, got logs: {logs:?}"
        );
    }

    /// Positive control for the test above: an ordinary EMPTY directory is not
    /// worth a word, so the check cannot be passing by warning about everything.
    #[test]
    fn verify_stays_quiet_about_an_empty_non_write_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let bare = dir.path().to_string_lossy().into_owned();
        let targets = vec![target_at("not-in-this-run", &bare, None)];
        let progress = TestProgress::new();

        assert!(verify_write_targets(&targets, &[], &progress).is_ok());
        assert!(
            progress.logs.lock().unwrap().is_empty(),
            "an empty bare directory must not produce any log line"
        );
    }

    #[test]
    fn partition_device_primary() {
        assert_eq!(
            partition_device("/dev/sdb", &TargetRole::Primary),
            "/dev/sdb1"
        );
    }

    #[test]
    fn partition_device_mirror() {
        assert_eq!(
            partition_device("/dev/sdc", &TargetRole::Mirror),
            "/dev/sdc2"
        );
    }

    #[test]
    fn mount_error_display() {
        let err = MountError::NoDrivesFound;
        assert!(err.to_string().contains("powered on"));

        let err = MountError::DriveNotFound {
            label: "test".into(),
            serial: "ABC".into(),
        };
        assert!(err.to_string().contains("ABC"));

        let err = MountError::PartitionNotFound {
            label: "test".into(),
            partition: "/dev/sdb1".into(),
        };
        assert!(err.to_string().contains("/dev/sdb1"));
    }

    #[test]
    fn mount_guard_count_empty() {
        let guard = MountGuard::new(ScriptedRunner::succeeding());
        assert_eq!(guard.count(), 0);
    }

    #[test]
    fn mount_guard_tracks_mounts() {
        let runner = ScriptedRunner::succeeding();
        let guard = guard_holding(&runner, &["/mnt/test1", "/mnt/test2"]);
        assert_eq!(guard.count(), 2);
    }

    #[test]
    fn ensure_targets_empty_config() {
        use crate::config::Config;
        use crate::progress::NullProgress;

        let config = Config::default();
        let progress = NullProgress;
        let result = ensure_targets_mounted(
            &config,
            raid_guard::Job::Backup,
            &progress,
            &MaintenanceHeld::assumed(),
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap().count(), 0);
    }

    #[test]
    fn fail_if_still_mounted_keeps_a_clean_result_and_fails_a_dirty_one() {
        let none: &[String] = &[];
        let held = ["/mnt/t".to_string()];
        assert_eq!(fail_if_still_mounted(Ok::<i32, String>(7), none), Ok(7));
        assert_eq!(
            fail_if_still_mounted(Err::<i32, String>("boom".into()), none),
            Err("boom".to_string())
        );
        assert_eq!(
            fail_if_still_mounted(Ok::<i32, String>(7), &held),
            Err("still mounted: /mnt/t".to_string())
        );
        assert_eq!(
            fail_if_still_mounted(Err::<i32, String>("boom".into()), &held),
            Err("boom; still mounted: /mnt/t".to_string())
        );
        assert_eq!(require_released(none), Ok(()));
        assert_eq!(
            require_released(&held),
            Err("still mounted: /mnt/t".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // A recovery drive's ESP (bd DAS-Backup-Manager-sp78.20)
    // -----------------------------------------------------------------------

    const ESP_PU: &str = "5eed0025-0000-4000-8000-000000000025";

    fn esp_target(partuuid: Option<&str>) -> Target {
        Target {
            label: "recovery-a".into(),
            serial: String::new(),
            serials: vec!["SERIAL04".into()],
            mount_uuid: Some("5eed0008-0000-4000-8000-000000000008".into()),
            mount: "/mnt/recovery-a".into(),
            role: TargetRole::Mirror,
            retention: Retention::default(),
            display_name: String::new(),
            esp_partuuid: partuuid.map(str::to_string),
        }
    }

    /// The drive's partitions as `lsblk` names them; the ESP's PARTUUID in
    /// upper case, as some tools print it.
    fn esp_drive_ids() -> std::collections::BTreeSet<String> {
        [
            "LABEL=RECOV-ESP-1".to_string(),
            "UUID=ABCD-0003".into(),
            format!("PARTUUID={}", ESP_PU.to_uppercase()),
            format!("PARTUUID-DEV={}@8:17", ESP_PU.to_uppercase()),
            "UUID=5eed0008-0000-4000-8000-000000000008".into(),
            "PARTUUID=5eed0026-0000-4000-8000-000000000026".into(),
        ]
        .into_iter()
        .collect()
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// Runs the ESP reading against `runner`, under a scratch root, with a
    /// reader that records the directory it was handed.
    fn read_esp(
        runner: &ScriptedRunner,
        root: &Path,
        target: &Target,
        answer: Result<Vec<String>, String>,
    ) -> (Result<Vec<String>, String>, Vec<PathBuf>) {
        let seen = Mutex::new(Vec::new());
        let read = |dir: &Path| {
            seen.lock().unwrap().push(dir.to_path_buf());
            answer.clone()
        };
        let host = esp_by_partuuid_host("8:17");
        let r = host.with_clear(runner, |c| {
            read_recovery_esp_with(
                runner,
                root,
                c,
                Path::new(BY_PU),
                target,
                &esp_drive_ids(),
                &read,
            )
        });
        assert_eq!(
            r.left_mounted, None,
            "left mounted: use read_esp_reading for that case"
        );
        (r.read, seen.into_inner().unwrap())
    }

    fn argvs(runner: &ScriptedRunner, program: &str) -> Vec<Vec<String>> {
        runner
            .calls()
            .into_iter()
            .filter(|c| c.argv[0] == program)
            .map(|c| c.argv)
            .collect()
    }

    /// `findmnt -o PARTUUID,MAJ:MIN` on the ESP's directory answers this
    /// partition and the drive's device number for it, so the mount is shown
    /// to be it.
    fn verified() -> (&'static str, Reply) {
        verified_as(ESP_PU, "8:17")
    }

    fn verified_as(pu: &str, dev: &str) -> (&'static str, Reply) {
        (
            "PARTUUID,MAJ:MIN",
            Reply::Exit {
                code: 0,
                stdout: leak(format!("PARTUUID=\"{pu}\" MAJ:MIN=\"{dev}\"\n")),
                stderr: "",
            },
        )
    }

    /// Where the tests' `/dev/disk/by-partuuid` is.
    const BY_PU: &str = "/nonexistent-by-partuuid";

    /// A host on which the ESP's by-partuuid path is the block device `dev`.
    fn esp_by_partuuid_host(dev: &str) -> Host {
        Host {
            block_devs: vec![(format!("{BY_PU}/{ESP_PU}"), dev.to_string())],
            ..Host::default()
        }
    }

    /// `findmnt -S` and `mountpoint -q` find nothing: the ESP is mounted
    /// nowhere and its directory is no mount point.
    fn unmounted_host() -> [(&'static str, Reply); 2] {
        [("-S", Reply::exit(1)), ("-q", Reply::exit(32))]
    }

    #[test]
    fn the_esp_is_mounted_read_only_read_and_unmounted() {
        let scratch = tempfile::tempdir().unwrap();
        let [s, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[s, q, verified()]);
        let (r, seen) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(ESP_PU)),
            Ok(vec!["a problem".into()]),
        );
        let dir = scratch.path().join("recovery-a");
        let dir_s = dir.to_str().unwrap();
        assert_eq!(r, Ok(vec!["a problem".to_string()]));
        assert_eq!(seen, vec![dir.clone()]);
        assert_eq!(
            argvs(&runner, "mount"),
            vec![vec![
                "mount".to_string(),
                "-t".into(),
                "vfat".into(),
                "-o".into(),
                "ro,nodev,nosuid,noexec,noatime".into(),
                format!("{BY_PU}/{ESP_PU}"),
                dir_s.into(),
            ]],
            "the by-partuuid path shown to be the drive's partition, never PARTUUID="
        );
        assert_eq!(
            argvs(&runner, "umount"),
            vec![vec!["umount".to_string(), dir_s.into()]]
        );
        assert!(
            !dir.exists(),
            "its directory is removed once it is unmounted"
        );
    }

    #[test]
    fn a_partuuid_this_drive_does_not_list_is_never_mounted() {
        // The host's EFI, copied into the key by mistake: refused before
        // the host is asked anything.
        let scratch = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::succeeding();
        let host_efi = "5eed0099-0000-4000-8000-000000000099";
        let (r, seen) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(host_efi)),
            Ok(vec![]),
        );
        let why = r.unwrap_err();
        assert!(
            why.contains(host_efi) && why.contains("not a partition of this drive"),
            "{why}"
        );
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert!(seen.is_empty());
        // The drive's own second partition is listed, but is no PARTUUID of
        // a key that is malformed.
        let (r, _) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some("RECOV-ESP-1")),
            Ok(vec![]),
        );
        assert!(r.is_err() && runner.calls().is_empty(), "{r:?}");
    }

    #[test]
    fn an_upper_case_key_is_asked_for_in_lower_case() {
        let scratch = tempfile::tempdir().unwrap();
        let [s, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[s, q, verified()]);
        let (r, _) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(&ESP_PU.to_uppercase())),
            Ok(vec![]),
        );
        assert_eq!(r, Ok(vec![]));
        let spec = format!("PARTUUID={ESP_PU}");
        assert!(
            argvs(&runner, "mount")[0].contains(&format!("{BY_PU}/{ESP_PU}")),
            "{:?}",
            runner.calls()
        );
        assert!(
            argvs(&runner, "findmnt")[0].contains(&spec),
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn a_target_without_the_key_is_not_checked_and_nothing_is_run() {
        let scratch = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::succeeding();
        let (r, seen) = read_esp(&runner, scratch.path(), &esp_target(None), Ok(vec![]));
        assert!(r.unwrap_err().contains("esp_partuuid"));
        assert!(runner.calls().is_empty() && seen.is_empty());
    }

    /// Another program's mount of the ESP, on a described host: udisks has
    /// it at [`ESP_AT`] (mount id 50, device 8:17), `holder` uses it, the
    /// reading is a job that writes nothing (it may not stop readers).
    fn read_esp_elsewhere(
        holder: Option<crate::holders::testproc::Proc<'_>>,
    ) -> (Result<Vec<String>, String>, Arc<ScriptedRunner>, Host, bool) {
        let scratch = tempfile::tempdir().unwrap();
        let pdir = scratch.path().join("by-partuuid");
        let mut host = esp_mounted_by_udisks(holder);
        host.read_only_job = true;
        host.block_devs.push((
            pdir.join(ESP_PU).to_str().unwrap().to_string(),
            "8:17".to_string(),
        ));
        let [_, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[
            (
                "-S",
                Reply::Exit {
                    code: 0,
                    stdout: leak(format!("{ESP_AT}\n")),
                    stderr: "",
                },
            ),
            q,
            verified(),
        ]);
        let seen = Mutex::new(false);
        let read = |_: &Path| {
            *seen.lock().unwrap() = true;
            Ok(vec!["found".to_string()])
        };
        let r = host.with_clear(&runner, |c| {
            read_recovery_esp_with(
                &*runner,
                scratch.path(),
                c,
                &pdir,
                &esp_target(Some(ESP_PU)),
                &esp_drive_ids(),
                &read,
            )
        });
        let read_ran = *seen.lock().unwrap();
        (r.read, runner, host, read_ran)
    }

    /// bd eg4j.19: another program's idle mount of the ESP is unmounted (by
    /// the stop policy of a read-only job) and the reading goes on.
    #[test]
    fn esp_mounted_elsewhere_idle_is_unmounted_and_read() {
        let (r, runner, host, read_ran) = read_esp_elsewhere(None);
        assert_eq!(r, Ok(vec!["found".to_string()]));
        assert!(read_ran);
        assert_eq!(
            argvs(&runner, "umount")[0],
            vec!["umount", "--no-canonicalize", ESP_AT]
        );
        assert_eq!(
            argvs(&runner, "mount").len(),
            1,
            "then its own read-only mount"
        );
        assert!(host.signals().is_empty());
    }

    /// A reader keeps another program's mount: not read, the reader named,
    /// nothing signalled, nothing unmounted (a read-only job never stops one).
    #[test]
    fn esp_mounted_elsewhere_held_by_reader_not_read_nothing_signalled() {
        let (r, runner, host, read_ran) = read_esp_elsewhere(Some(esp_user(300, "0100000")));
        let why = r.unwrap_err();
        assert!(
            why.contains("pid 300")
                && why.contains("never stops a process")
                && why.contains(ESP_HELD),
            "{why}"
        );
        assert!(!read_ran);
        assert!(host.signals().is_empty());
        assert!(argvs(&runner, "umount").is_empty() && argvs(&runner, "mount").is_empty());
    }

    /// Review N2 (bd eg4j.19): a dry run (`recovery-os status --report-only`)
    /// says what it would unmount, unmounts nothing, and does not read the
    /// ESP beside another program's mount.
    #[test]
    fn a_dry_run_reading_says_it_would_unmount_and_reads_nothing() {
        let scratch = tempfile::tempdir().unwrap();
        let pdir = scratch.path().join("by-partuuid");
        let mut host = esp_mounted_by_udisks(None);
        host.read_only_job = true;
        host.report_only = true;
        host.block_devs.push((
            pdir.join(ESP_PU).to_str().unwrap().to_string(),
            "8:17".to_string(),
        ));
        let [_, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[
            (
                "-S",
                Reply::Exit {
                    code: 0,
                    stdout: leak(format!("{ESP_AT}\n")),
                    stderr: "",
                },
            ),
            q,
            verified(),
        ]);
        let seen = Mutex::new(false);
        let read = |_: &Path| {
            *seen.lock().unwrap() = true;
            Ok(vec!["found".to_string()])
        };
        let r = host.with_clear(&runner, |c| {
            read_recovery_esp_with(
                &*runner,
                scratch.path(),
                c,
                &pdir,
                &esp_target(Some(ESP_PU)),
                &esp_drive_ids(),
                &read,
            )
        });
        let why = r.read.unwrap_err();
        assert!(
            why.contains(&format!("would unmount {ESP_AT}")) && why.contains("not read"),
            "{why}"
        );
        assert!(!*seen.lock().unwrap());
        assert!(argvs(&runner, "umount").is_empty() && argvs(&runner, "mount").is_empty());
        assert!(host.signals().is_empty());
    }

    /// N2: a dry run finding a mount an earlier reading left at its own
    /// directory says it would unmount it, unmounts nothing, reads nothing
    /// (the spec's anti-goal: a dry run changes nothing).
    #[test]
    fn a_dry_run_leaves_an_earlier_reading_s_mount_and_reads_nothing() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("recovery-a");
        let mut host = esp_by_partuuid_host("8:17");
        host.report_only = true;
        let runner = ScriptedRunner::with_rules(&[
            (
                "-S",
                Reply::Exit {
                    code: 0,
                    stdout: leak(format!("{}\n", dir.display())),
                    stderr: "",
                },
            ),
            ("-q", Reply::exit(32)),
            verified(),
        ]);
        let (r, ran) = read_esp_on(&host, &runner, scratch.path(), &esp_drive_ids());
        let why = r.read.unwrap_err();
        assert!(
            why.contains(&format!("would unmount {}", dir.display())) && why.contains("not read"),
            "{why}"
        );
        assert!(!ran);
        assert!(argvs(&runner, "umount").is_empty() && argvs(&runner, "mount").is_empty());
    }

    #[test]
    fn esp_mounted_elsewhere_held_by_a_writer_is_not_read_and_not_signalled() {
        let (r, runner, host, read_ran) = read_esp_elsewhere(Some(esp_user(301, "0100002")));
        let why = r.unwrap_err();
        assert!(why.contains("pid 301") && why.contains(ESP_HELD), "{why}");
        assert!(!read_ran);
        assert!(host.signals().is_empty());
        assert!(argvs(&runner, "umount").is_empty());
    }

    /// The partition's PARTUUID path is no block device (the drive went):
    /// nothing is unmounted on a guess; not a "held" finding.
    #[test]
    fn an_esp_mounted_elsewhere_whose_partition_is_no_block_device_is_not_touched() {
        let scratch = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::with_rules(&[(
            "-S",
            Reply::Exit {
                code: 0,
                stdout: "/media/someone\n",
                stderr: "",
            },
        )]);
        // No block device at the by-partuuid path on this host.
        let (r, ran) = read_esp_on(&Host::default(), &runner, scratch.path(), &esp_drive_ids());
        let why = r.read.unwrap_err();
        assert!(
            why.contains("/media/someone") && why.contains("not a block device"),
            "{why}"
        );
        assert!(!why.contains(ESP_HELD), "{why}");
        assert!(argvs(&runner, "mount").is_empty() && argvs(&runner, "umount").is_empty());
        assert!(!ran);
    }

    // --- review B1 (bd eg4j.19): the ESP is the drive's own partition by
    // its device number, never by what `/dev/disk/by-partuuid` points at ---

    /// The reading on `host` (its block devices and mount table) with the
    /// drive's ids `ids`; whether `read` ran.
    fn read_esp_on(
        host: &Host,
        runner: &ScriptedRunner,
        root: &Path,
        ids: &std::collections::BTreeSet<String>,
    ) -> (EspReading<Vec<String>>, bool) {
        let seen = Mutex::new(false);
        let read = |_: &Path| {
            *seen.lock().unwrap() = true;
            Ok(vec!["found".to_string()])
        };
        let r = host.with_clear(runner, |c| {
            read_recovery_esp_with(
                runner,
                root,
                c,
                Path::new(BY_PU),
                &esp_target(Some(ESP_PU)),
                ids,
                &read,
            )
        });
        let ran = *seen.lock().unwrap();
        (r, ran)
    }

    /// Another attached partition carries the same PARTUUID (a cloned GPT)
    /// and is the host's `/boot`; by-partuuid resolves to IT (259:5), not to
    /// the drive's partition (8:17). Nothing is cleared, nothing mounted.
    #[test]
    fn a_cloned_partuuid_on_another_disk_mounted_as_boot_is_never_unmounted() {
        let scratch = tempfile::tempdir().unwrap();
        let mut host = esp_by_partuuid_host("259:5");
        host.mountinfo = vec![
            "22 1 0:21 / / rw - btrfs /dev/root0 rw".into(),
            "40 22 259:5 / /boot rw - vfat /dev/nvme0n1p3 rw".into(),
        ];
        host.block_devs
            .push(("/dev/nvme0n1p3".into(), "259:5".into()));
        let runner = ScriptedRunner::with_rules(&[
            (
                "-S",
                Reply::Exit {
                    code: 0,
                    stdout: "/boot\n",
                    stderr: "",
                },
            ),
            ("-q", Reply::exit(32)),
            verified(),
        ]);
        let (r, ran) = read_esp_on(&host, &runner, scratch.path(), &esp_drive_ids());
        let why = r.read.unwrap_err();
        assert!(
            why.contains("259:5") && why.contains("8:17") && why.contains("not read"),
            "{why}"
        );
        assert!(!ran);
        assert!(
            argvs(&runner, "umount").is_empty() && argvs(&runner, "mount").is_empty(),
            "{:?}",
            runner.calls()
        );
        assert!(host.signals().is_empty());
    }

    /// Mounted nowhere, but by-partuuid resolves to another disk's partition
    /// with the same PARTUUID: never mounted.
    #[test]
    fn a_by_partuuid_path_that_is_not_the_drive_s_partition_is_never_mounted() {
        let scratch = tempfile::tempdir().unwrap();
        let host = esp_by_partuuid_host("259:5");
        let [s, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[s, q, verified()]);
        let (r, ran) = read_esp_on(&host, &runner, scratch.path(), &esp_drive_ids());
        let why = r.read.unwrap_err();
        assert!(why.contains("259:5") && why.contains("8:17"), "{why}");
        assert!(!ran && argvs(&runner, "mount").is_empty());
    }

    /// The drive's listing pairs no device number with the PARTUUID: not
    /// read, nothing asked of the host.
    #[test]
    fn no_paired_device_number_is_not_read_and_nothing_is_run() {
        let scratch = tempfile::tempdir().unwrap();
        let host = esp_by_partuuid_host("8:17");
        let runner = ScriptedRunner::succeeding();
        let ids: std::collections::BTreeSet<String> = esp_drive_ids()
            .into_iter()
            .filter(|i| !i.starts_with("PARTUUID-DEV="))
            .collect();
        let (r, ran) = read_esp_on(&host, &runner, scratch.path(), &ids);
        let why = r.read.unwrap_err();
        assert!(
            why.contains("device number") && why.contains("not read"),
            "{why}"
        );
        assert!(!ran);
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    /// findmnt reports the PARTUUID mounted elsewhere, but no mount of the
    /// drive's partition (by device number) is there: the mount is another
    /// partition's; never mounted beside it.
    #[test]
    fn a_mount_elsewhere_that_is_not_the_drive_s_partition_stops_the_reading() {
        let scratch = tempfile::tempdir().unwrap();
        let mut host = esp_by_partuuid_host("8:17");
        host.mountinfo = vec![
            "22 1 0:21 / / rw - btrfs /dev/root0 rw".into(),
            "40 22 259:5 / /boot rw - vfat /dev/nvme0n1p3 rw".into(),
        ];
        let runner = ScriptedRunner::with_rules(&[
            (
                "-S",
                Reply::Exit {
                    code: 0,
                    stdout: "/boot\n",
                    stderr: "",
                },
            ),
            ("-q", Reply::exit(32)),
            verified(),
        ]);
        let (r, ran) = read_esp_on(&host, &runner, scratch.path(), &esp_drive_ids());
        let why = r.read.unwrap_err();
        assert!(why.contains("/boot") && why.contains("not read"), "{why}");
        assert!(!ran);
        assert!(argvs(&runner, "umount").is_empty() && argvs(&runner, "mount").is_empty());
    }

    /// The mount made is shown, after it, to be another device than the
    /// drive's (the link moved between check and mount): not read, our own
    /// mount taken down.
    #[test]
    fn a_mount_whose_device_number_is_not_the_drive_s_is_unmounted_unread() {
        let scratch = tempfile::tempdir().unwrap();
        let host = esp_by_partuuid_host("8:17");
        let [s, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[s, q, verified_as(ESP_PU, "259:5")]);
        let (r, ran) = read_esp_on(&host, &runner, scratch.path(), &esp_drive_ids());
        let why = r.read.unwrap_err();
        assert!(why.contains("259:5") && why.contains("not read"), "{why}");
        assert!(!ran);
        assert_eq!(argvs(&runner, "umount").len(), 1);
    }

    #[test]
    fn something_else_mounted_on_its_directory_is_refused() {
        let scratch = tempfile::tempdir().unwrap();
        let runner =
            ScriptedRunner::with_rules(&[("-S", Reply::exit(1)), ("-q", Reply::ok()), verified()]);
        let (r, seen) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(ESP_PU)),
            Ok(vec![]),
        );
        assert!(r.unwrap_err().contains("already a mount point"));
        assert!(argvs(&runner, "mount").is_empty() && argvs(&runner, "umount").is_empty());
        assert!(seen.is_empty());
    }

    #[test]
    fn an_esp_an_earlier_reading_left_mounted_is_taken_down_and_mounted_afresh() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("recovery-a");
        let runner = ScriptedRunner::with_rules(&[
            (
                "-S",
                Reply::Exit {
                    code: 0,
                    stdout: leak(format!("{}\n", dir.display())),
                    stderr: "",
                },
            ),
            ("-q", Reply::exit(32)),
            verified(),
        ]);
        let (r, seen) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(ESP_PU)),
            Ok(vec![]),
        );
        assert_eq!(r, Ok(vec![]));
        assert_eq!(seen.len(), 1);
        let calls: Vec<String> = runner
            .calls()
            .into_iter()
            .filter(|c| c.argv[0] == "mount" || c.argv[0] == "umount")
            .map(|c| c.argv[0].clone())
            .collect();
        assert_eq!(calls, ["umount", "mount", "umount"]);
    }

    #[test]
    fn a_leftover_that_will_not_come_down_is_not_mounted_over() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("recovery-a");
        let runner = ScriptedRunner::with_rules(&[
            (
                "-S",
                Reply::Exit {
                    code: 0,
                    stdout: leak(format!("{}\n", dir.display())),
                    stderr: "",
                },
            ),
            ("umount", Reply::exit(32)),
        ]);
        let (r, seen) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(ESP_PU)),
            Ok(vec![]),
        );
        assert!(r.unwrap_err().contains("umount -f"));
        assert!(argvs(&runner, "mount").is_empty() && seen.is_empty());
    }

    #[test]
    fn a_failed_read_still_unmounts() {
        let scratch = tempfile::tempdir().unwrap();
        let [s, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[s, q, verified()]);
        let (r, _) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(ESP_PU)),
            Err("loader/entries: I/O error".into()),
        );
        assert_eq!(r, Err("loader/entries: I/O error".to_string()));
        assert_eq!(argvs(&runner, "umount").len(), 1);
    }

    #[test]
    fn a_mount_that_is_not_the_esp_is_unmounted_unread() {
        let scratch = tempfile::tempdir().unwrap();
        let [s, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[
            s,
            q,
            verified_as("5eed0026-0000-4000-8000-000000000026", "8:17"),
        ]);
        let (r, seen) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(ESP_PU)),
            Ok(vec![]),
        );
        assert!(r.unwrap_err().contains("5eed0026"));
        assert!(seen.is_empty());
        assert_eq!(argvs(&runner, "umount").len(), 1);
    }

    #[test]
    fn a_failed_mount_is_not_read_and_its_directory_removed() {
        let scratch = tempfile::tempdir().unwrap();
        let [s, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[s, q, ("mount", Reply::exit(32))]);
        let (r, seen) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(ESP_PU)),
            Ok(vec![]),
        );
        assert!(r.unwrap_err().contains("mount exited with code 32"));
        assert!(seen.is_empty() && argvs(&runner, "umount").is_empty());
        assert!(!scratch.path().join("recovery-a").exists());
    }

    /// bd DAS-Backup-Manager-s5jv.2: what the reading found is kept when the
    /// ESP cannot be taken down after it; that it stays mounted is said beside
    /// it, never in its place.
    #[test]
    fn an_esp_left_mounted_after_its_reading_keeps_the_findings_and_says_so() {
        let scratch = tempfile::tempdir().unwrap();
        let [s, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[s, q, verified(), ("umount", Reply::exit(32))]);
        let host = esp_by_partuuid_host("8:17");
        let r = host.with_clear(&runner, |c| {
            read_recovery_esp_with(
                &*runner,
                scratch.path(),
                c,
                Path::new(BY_PU),
                &esp_target(Some(ESP_PU)),
                &esp_drive_ids(),
                &|_: &Path| Ok(vec!["a problem".to_string()]),
            )
        });
        assert_eq!(r.read, Ok(vec!["a problem".to_string()]));
        let why = r.left_mounted.unwrap();
        assert!(why.contains("still mounted"), "{why}");
        assert_eq!(argvs(&runner, "umount").len(), UMOUNT_ATTEMPTS as usize);
        assert!(
            scratch.path().join("recovery-a").exists(),
            "a mounted directory is kept"
        );
    }

    #[test]
    fn a_label_that_is_no_safe_file_name_does_not_escape_the_root() {
        let scratch = tempfile::tempdir().unwrap();
        let [s, q] = unmounted_host();
        let runner = ScriptedRunner::with_rules(&[s, q, verified()]);
        let mut t = esp_target(Some(ESP_PU));
        t.label = "../../etc".into();
        let (_, seen) = read_esp(&runner, scratch.path(), &t, Ok(vec![]));
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].parent(), Some(scratch.path()), "{seen:?}");
    }

    /// bd DAS-Backup-Manager-s5jv.2: an ESP a reading left mounted is
    /// released by the job's own unmount, before the targets, and one that
    /// will not come down is still mounted, so the job fails as for a target.
    #[test]
    fn the_job_s_unmount_releases_an_esp_a_reading_left_mounted_before_the_targets() {
        let root = Path::new("/run/das-backup/esp");
        let dir = "/run/das-backup/esp/recovery-a";
        let spec = format!("PARTUUID={ESP_PU}");
        let shown = |stdout: &'static str| {
            (
                leak(spec.clone()),
                Reply::Exit {
                    code: 0,
                    stdout,
                    stderr: "",
                },
            )
        };
        // Shown there: unmounted, then the target.
        let runner = ScriptedRunner::with_rules(&[shown(leak(format!("{dir}\n")))]);
        let mut guard = MountGuard::new(runner.clone());
        guard.newly_mounted.push("/mnt/recovery-a".into());
        guard.note_esp(&esp_target(Some(ESP_PU)), root);
        assert!(guard.unmount(&Recorder::default()).is_empty());
        assert_eq!(
            argvs(&runner, "umount"),
            [
                vec!["umount".to_string(), dir.into()],
                vec!["umount".to_string(), "/mnt/recovery-a".into()]
            ]
        );
        // Shown elsewhere, or nowhere: not this project's, never unmounted.
        let runner = ScriptedRunner::with_rules(&[shown("/somewhere/else\n")]);
        let mut guard = MountGuard::new(runner.clone());
        guard.note_esp(&esp_target(Some(ESP_PU)), root);
        assert!(guard.unmount(&Recorder::default()).is_empty());
        assert!(argvs(&runner, "umount").is_empty(), "{:?}", runner.calls());
        // Will not come down: still mounted, the plain tries then -f.
        let runner =
            ScriptedRunner::with_rules(&[shown(leak(format!("{dir}\n"))), (dir, Reply::exit(32))]);
        let mut guard = MountGuard::new(runner.clone());
        guard.note_esp(&esp_target(Some(ESP_PU)), root);
        assert_eq!(guard.unmount(&Recorder::default()), [dir.to_string()]);
        assert_eq!(argvs(&runner, "umount").len(), UMOUNT_ATTEMPTS as usize + 1);
        // A primary, or a mirror with no key, has no ESP to release.
        let mut primary = esp_target(Some(ESP_PU));
        primary.role = TargetRole::Primary;
        let mut guard = MountGuard::new(ScriptedRunner::succeeding());
        guard.note_esp(&primary, root);
        guard.note_esp(&esp_target(None), root);
        assert!(guard.esps.is_empty());
    }

    /// What `findmnt -S` says decides what is released: 1 is "mounted
    /// nowhere" (nothing to do, nothing held); any other failure, or no
    /// findmnt at all, cannot tell, so the directory counts as still mounted
    /// and the job fails — nothing is unmounted on a guess (bd s5jv.2).
    #[test]
    fn an_esp_findmnt_cannot_answer_for_is_never_unmounted_and_fails_the_job() {
        let root = Path::new("/run/das-backup/esp");
        let dir = "/run/das-backup/esp/recovery-a".to_string();
        let spec = leak(format!("PARTUUID={ESP_PU}"));
        for (reply, still) in [
            (Reply::exit(1), Vec::<String>::new()),
            (Reply::exit(2), vec![dir.clone()]),
            (Reply::CannotRun, vec![dir.clone()]),
        ] {
            let runner = ScriptedRunner::with_rules(&[(spec, reply.clone())]);
            let mut guard = MountGuard::new(runner.clone());
            guard.note_esp(&esp_target(Some(ESP_PU)), root);
            assert_eq!(guard.unmount(&Recorder::default()), still, "{reply:?}");
            assert!(
                argvs(&runner, "umount").is_empty(),
                "{reply:?}: {:?}",
                runner.calls()
            );
        }
    }

    /// Under a handed-down lock the parent (`backup-run.sh`) releases the
    /// ESPs itself in `unmount_all`; an owned hold notes them (bd s5jv.2).
    #[test]
    fn only_an_owned_hold_notes_the_mirrors_esps_for_release() {
        let scratch = Scratch::new();
        scratch.touch("dev/sdb2");
        let mnt = scratch.path("mnt/a");
        let mut t = target("recovery-a", "SER-A", &mnt, TargetRole::Mirror);
        t.esp_partuuid = Some(ESP_PU.into());
        let config = config_with(vec![t], Vec::new());
        let host = Host {
            devices: vec![("SER-A".into(), scratch.path("dev/sdb"))],
            ..Host::default()
        };
        let guard = host
            .mount_targets(&config, &Recorder::default(), &ScriptedRunner::succeeding())
            .unwrap();
        assert_eq!(guard.esps.len(), 1);
        let guard = host
            .mount_targets_held(
                &config,
                &Recorder::default(),
                &ScriptedRunner::succeeding(),
                &MaintenanceHeld::delegated_for_test(),
            )
            .unwrap();
        assert!(guard.esps.is_empty());
    }

    /// bd DAS-Backup-Manager-1fpw: a `mount` that exits non-zero yet mounted
    /// is this job's mount, taken down — never left behind untracked, as the
    /// guard recorded only a mount that exited 0 (`backup-run.sh` records
    /// ownership before mounting).
    #[test]
    fn a_target_mount_that_failed_but_mounted_is_taken_down() {
        let scratch = Scratch::new();
        let (config, host, _part, mnt) = attached_primary(&scratch);
        let runner = ScriptedRunner::with_rules(&[("mount", Reply::FailMounting)]);
        let progress = Recorder::default();
        let r = host.mount_targets(&config, &progress, &runner);
        drop(r);
        assert!(
            runner.mounted.lock().unwrap().is_empty(),
            "left mounted: {:?}",
            runner.calls()
        );
        assert!(
            argvs(&runner, "umount")
                .iter()
                .any(|a| a.last() == Some(&mnt))
        );
    }

    #[test]
    fn a_source_mount_that_failed_but_mounted_is_taken_down() {
        let scratch = Scratch::new();
        let vol = scratch.mkdir("vol");
        let config = config_with(Vec::new(), vec![source("s", "/dev/fake-hdd", &vol)]);
        let runner = ScriptedRunner::with_rules(&[("mount", Reply::FailMounting)]);
        let progress = Recorder::default();
        let guard = Host::default().mount_sources(&config, &progress, &runner);
        drop(guard);
        assert!(
            runner.mounted.lock().unwrap().is_empty(),
            "left mounted: {:?}",
            runner.calls()
        );
    }

    // --- bd DAS-Backup-Manager-s5jv.3 ------------------------------------

    #[test]
    fn a_label_of_dots_never_names_the_esp_root_or_its_parent() {
        assert_eq!(esp_dir_name("."), "_.");
        assert_eq!(esp_dir_name(".."), "_..");
        assert_eq!(esp_dir_name(""), "_");
        assert_eq!(esp_dir_name("recovery-a"), "recovery-a");
        assert_eq!(esp_dir_name("a/../b"), "a_.._b");
    }

    #[test]
    fn a_mountpoint_check_that_gives_no_answer_mounts_nothing_and_leaves_no_directory() {
        // Exit 1 is neither "not a mount point" (32) nor "is one" (0).
        let scratch = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::with_rules(&[("-S", Reply::exit(1)), ("-q", Reply::exit(1))]);
        let (r, seen) = read_esp(
            &runner,
            scratch.path(),
            &esp_target(Some(ESP_PU)),
            Ok(vec![]),
        );
        let why = r.unwrap_err();
        assert!(
            why.contains("mountpoint exited with code 1") && !why.contains("already a mount point"),
            "{why}"
        );
        assert!(argvs(&runner, "mount").is_empty());
        assert!(seen.is_empty());
        assert!(!scratch.path().join("recovery-a").exists());
    }

    // -----------------------------------------------------------------------
    // bd DAS-Backup-Manager-su1b.34: the survivors of the CI mutation run.
    // -----------------------------------------------------------------------

    /// The scripted answers a `raid1_mounted_by_udisks` host needs, with
    /// `umount` answering `umount` instead.
    fn udisks_runner(scratch: &Scratch, umount: Reply) -> Arc<ScriptedRunner> {
        let (part_b, part_c) = (scratch.path("dev/sdb1"), scratch.path("dev/sdc1"));
        ScriptedRunner::with_rules(&[
            ("blkid", Reply::Out(0, format!("{part_b}\n{part_c}\n"))),
            (
                &part_b,
                Reply::Out(
                    0,
                    super_of("u-p", 1, "5eed00a1-0000-4000-8000-0000000000a1", 40),
                ),
            ),
            (
                &part_c,
                Reply::Out(
                    0,
                    super_of("u-p", 2, "5eed00a2-0000-4000-8000-0000000000a2", 40),
                ),
            ),
            ("umount", umount),
        ])
    }

    /// A process that exists but holds nothing of the mount.
    fn idle(pid: i32) -> crate::holders::testproc::Proc<'static> {
        crate::holders::testproc::Proc {
            pid,
            cmd: "idle",
            ..Default::default()
        }
    }

    #[test]
    fn the_mount_table_read_is_this_processs_own() {
        let text = read_self_mountinfo().unwrap();
        let entries = crate::holders::parse_mountinfo(&text);
        assert!(
            entries.iter().any(|m| m.mount_point == "/"),
            "no root mount in {text:?}"
        );
        assert!(text.lines().count() >= 2, "{text:?}");
    }

    #[test]
    fn the_origin_of_a_mount_comes_from_systemctl_only_when_it_ran_and_succeeded() {
        use crate::holders::Origin;
        let shown = "Id=mnt-x.mount\nLoadState=loaded\nFragmentPath=/etc/systemd/system/mnt-x.mount\nSourcePath=\n";
        let unit = Origin::Unit {
            unit: "mnt-x.mount".into(),
            from: "/etc/systemd/system/mnt-x.mount".into(),
        };
        let ran = ScriptedRunner::with_rules(&[("systemctl", Reply::Out(0, shown.into()))]);
        assert_eq!(origin_of(ran.as_ref(), "/mnt/x"), unit);
        assert_eq!(
            ran.calls(),
            vec![output_call(&[
                "systemctl",
                "show",
                "--property=Id",
                "--property=LoadState",
                "--property=FragmentPath",
                "--property=SourcePath",
                "--",
                "/mnt/x"
            ])]
        );
        // The same text from a systemctl that failed is not believed.
        let failed = ScriptedRunner::with_rules(&[("systemctl", Reply::Out(1, shown.into()))]);
        assert_eq!(origin_of(failed.as_ref(), "/mnt/x"), Origin::Other);
        let absent = ScriptedRunner::with_rules(&[("systemctl", Reply::CannotRun)]);
        assert_eq!(origin_of(absent.as_ref(), "/mnt/x"), Origin::Other);
        assert_eq!(
            origin_of(failed.as_ref(), "/run/media/u/das"),
            Origin::Udisks
        );
    }

    /// `maintenance_on` against a described `/sys/fs/btrfs` and a scripted
    /// `btrfs scrub status`.
    fn maintenance(
        sysfs: &Path,
        fsid: Option<&str>,
        scrub: Reply,
    ) -> (Result<(), String>, Arc<ScriptedRunner>) {
        let runner = ScriptedRunner::with_rules(&[("scrub", scrub)]);
        let host = Host::default();
        let signals = HostSignals(&host);
        let none = || Ok(String::new());
        let site = ClearSite {
            mountinfo: &none,
            proc: crate::holders::ProcSite::host(),
            signaller: &signals,
            self_pid: 1,
            report_only: false,
            stop_readers: true,
            sysfs_btrfs: sysfs.to_path_buf(),
            block_dev: &|p: &str| Err(format!("{p}: no block devices in this test")),
        };
        let got = maintenance_on(runner.as_ref(), &site, fsid, "/mnt/t");
        (got, runner)
    }

    fn finished() -> Reply {
        Reply::Out(0, "UUID: u\nStatus:           finished\n".into())
    }

    #[test]
    fn an_idle_filesystem_has_no_maintenance_on_it() {
        let sys = tempfile::TempDir::new().unwrap();
        let fs_dir = sys.path().join("u-p");
        std::fs::create_dir_all(&fs_dir).unwrap();
        // "none", empty, and no file at all: nothing running.
        for text in [Some("none\n"), Some(""), Some("\n"), None] {
            if let Some(t) = text {
                std::fs::write(fs_dir.join("exclusive_operation"), t).unwrap();
            }
            let (got, runner) = maintenance(sys.path(), Some("U-P"), finished());
            assert_eq!(got, Ok(()), "{text:?}");
            assert_eq!(
                runner.calls(),
                vec![output_call(&["btrfs", "scrub", "status", "/mnt/t"])]
            );
        }
        // No filesystem UUID to look under: only the scrub is asked.
        let (got, _) = maintenance(sys.path(), None, finished());
        assert_eq!(got, Ok(()));
    }

    #[test]
    fn an_exclusive_operation_or_an_unreadable_one_refuses() {
        let sys = tempfile::TempDir::new().unwrap();
        let fs_dir = sys.path().join("u-p");
        std::fs::create_dir_all(&fs_dir).unwrap();
        std::fs::write(fs_dir.join("exclusive_operation"), "balance\n").unwrap();
        let (got, runner) = maintenance(sys.path(), Some("u-p"), finished());
        let e = got.unwrap_err();
        assert!(e.contains("a btrfs balance is running"), "{e}");
        assert!(runner.calls().is_empty(), "asked the scrub anyway");
        // Present but not a readable file: refused, never taken as none.
        std::fs::remove_file(fs_dir.join("exclusive_operation")).unwrap();
        std::fs::create_dir(fs_dir.join("exclusive_operation")).unwrap();
        let (got, runner) = maintenance(sys.path(), Some("u-p"), finished());
        let e = got.unwrap_err();
        assert!(
            e.contains("whether btrfs maintenance runs on it cannot be read"),
            "{e}"
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_running_scrub_or_one_that_cannot_be_asked_about_refuses() {
        let sys = tempfile::TempDir::new().unwrap();
        let ask = |reply| maintenance(sys.path(), None, reply).0;
        let running = "UUID: u\nScrub started: now\nStatus:           running\n";
        let e = ask(Reply::Out(0, running.into())).unwrap_err();
        assert!(e.contains("a scrub is running on it (/mnt/t)"), "{e}");
        // `running` anywhere but a Status line, and a Status that is not it.
        let text = "UUID: u\nStatus:           finished\nNote: not running\n";
        assert_eq!(ask(Reply::Out(0, text.into())), Ok(()));
        assert!(ask(Reply::Out(0, "Status:  running (was)\n".into())).is_err());
        // A status that failed is not read, even when it printed `running`.
        let e = ask(Reply::Out(1, running.into())).unwrap_err();
        assert!(e.contains("cannot be told"), "{e}");
        let e = ask(Reply::CannotRun).unwrap_err();
        assert!(e.contains("cannot be told (btrfs scrub status:"), "{e}");
    }

    /// `clear-target` for a leftover at the target's own path, with the
    /// holders `proc` describes.
    fn leftover_host(
        scratch: &Scratch,
        proc: Option<crate::holders::testproc::Proc<'static>>,
    ) -> (Config, Host, String) {
        let (mut config, mut host, _part, mnt) = attached_primary(scratch);
        config.targets[0].mount_uuid = Some("u-p".into());
        scratch.mkdir("mnt/p");
        host.mountpoints = vec![PathBuf::from(&mnt)];
        host.mountinfo = vec![format!("88 22 0:52 / {mnt} rw - btrfs /dev/sdb1 rw")];
        let table = crate::holders::testproc::FakeProc::new();
        if let Some(p) = proc {
            table.add(p);
        }
        host.proc = Some(Arc::new(table));
        (config, host, mnt)
    }

    fn is_target() -> Arc<ScriptedRunner> {
        ScriptedRunner::with_rules(&[(
            "UUID",
            Reply::Exit {
                code: 0,
                stdout: "u-p\n",
                stderr: "",
            },
        )])
    }

    fn leftover_reader() -> crate::holders::testproc::Proc<'static> {
        crate::holders::testproc::Proc {
            pid: 320,
            cmd: "less",
            fds: vec![(3, "/x", "0100000", Some(88))],
            ..Default::default()
        }
    }

    #[test]
    fn a_leftover_nothing_holds_says_so_and_one_with_a_reader_names_the_stop() {
        let scratch = Scratch::new();
        let (config, host, mnt) = leftover_host(&scratch, None);
        let got = clear_cli(&host, &is_target(), &config.targets[0], true);
        assert_eq!(got, ClearTarget::Cleared(format!("nothing holds {mnt}")));
        assert!(host.signals().is_empty());

        let scratch = Scratch::new();
        let (config, host, mnt) = leftover_host(&scratch, Some(leftover_reader()));
        let got = clear_cli(&host, &is_target(), &config.targets[0], true);
        assert_eq!(
            got,
            ClearTarget::Cleared(format!(
                "{STOPPED}: SIGTERM to pid 320 (less; user user, unit session-2.scope); \
                 nothing holds {mnt} now"
            ))
        );
        assert_eq!(host.signals(), vec![(320, crate::holders::Sig::Term)]);
    }

    #[test]
    fn a_leftover_dry_run_signals_nobody_and_says_so() {
        let scratch = Scratch::new();
        let (config, mut host, mnt) = leftover_host(&scratch, Some(leftover_reader()));
        host.report_only = true;
        let got = clear_cli(&host, &is_target(), &config.targets[0], true);
        assert_eq!(
            got,
            ClearTarget::Cleared(format!("dry run: nothing signalled at {mnt}"))
        );
        assert!(host.signals().is_empty(), "{:?}", host.signals());
    }

    #[test]
    fn another_programs_mount_is_cleared_for_the_script_and_each_outcome_is_worded() {
        // Held by nothing: unmounted, no signal.
        let scratch = Scratch::new();
        let (config, host, runner, _mnt) = raid1_mounted_by_udisks(&scratch, idle(300));
        let got = clear_cli(&host, &runner, &config.targets[0], false);
        assert_eq!(
            got,
            ClearTarget::Cleared(
                "another program's mount, held by nothing, was unmounted; the kernel has it \
                 mounted nowhere"
                    .into()
            )
        );
        assert!(host.signals().is_empty());
        assert!(runner.calls().contains(&status_call(&[
            "umount",
            "--no-canonicalize",
            "/run/media/u/das"
        ])));

        // Held by a reader: stopped, then unmounted.
        let scratch = Scratch::new();
        let (config, host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        let got = clear_cli(&host, &runner, &config.targets[0], false);
        assert_eq!(
            got,
            ClearTarget::Cleared(format!(
                "another program's mount was unmounted, {STOPPED}: SIGTERM to pid 300 \
                 (dolphin; user user, unit session-2.scope); the kernel has it mounted nowhere"
            ))
        );

        // Not mounted anywhere else: nothing to do.
        let scratch = Scratch::new();
        let (config, mut host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        host.mountinfo.truncate(1);
        let got = clear_cli(&host, &runner, &config.targets[0], false);
        assert_eq!(
            got,
            ClearTarget::Cleared("not mounted anywhere else".into())
        );
    }

    #[test]
    fn a_dry_run_clear_target_says_what_it_would_do_and_does_none_of_it() {
        let scratch = Scratch::new();
        let (config, mut host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        host.report_only = true;
        let got = clear_cli(&host, &runner, &config.targets[0], false);
        let ClearTarget::Cleared(text) = &got else {
            panic!("{got:?}");
        };
        assert!(
            text.starts_with("dry run: would unmount /run/media/u/das (udisks"),
            "{text}"
        );
        assert!(text.contains("would stop: pid 300"), "{text}");
        assert!(host.signals().is_empty(), "{:?}", host.signals());
        assert!(
            !runner.calls().iter().any(|c| c.argv[0] == "umount"),
            "{:?}",
            runner.calls()
        );
    }

    /// The first `umount` of another program's mount fails and the second
    /// works: one pause between them, after the first try, and no more.
    #[test]
    fn a_failed_umount_is_retried_once_after_its_pause() {
        let scratch = Scratch::new();
        let (config, host, _r, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        let runner = udisks_runner(&scratch, Reply::Seq(&[32, 0]));
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        let calls = runner.calls();
        let umounts: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c == &&status_call(&["umount", "--no-canonicalize", "/run/media/u/das"])
            })
            .map(|(i, _)| i)
            .collect();
        assert_eq!(umounts.len(), 2, "{calls:?}");
        let pauses = runner.pauses.lock().unwrap().clone();
        assert_eq!(pauses, vec![UMOUNT_RETRY_PAUSE]);
        // Between the first umount and the second, not before the first.
        let at = runner.pause_at.lock().unwrap().clone();
        assert!(
            at[0] > umounts[0] && at[0] <= umounts[1],
            "paused after {} commands; umounts at {umounts:?}",
            at[0]
        );
        assert_eq!(guard.count(), 1);
    }

    /// An `umount` that never works refuses the target after every attempt.
    #[test]
    fn a_mount_that_will_not_unmount_refuses_the_target_after_all_attempts() {
        let scratch = Scratch::new();
        let (config, host, _r, _mnt) = raid1_mounted_by_udisks(&scratch, idle(300));
        let runner = udisks_runner(&scratch, Reply::exit(32));
        let progress = Recorder::default();

        let result = host.mount_targets(&config, &progress, &runner);

        let Err(MountError::AllTargetsRefused { notes }) = result else {
            panic!("{:?}", result.err());
        };
        assert!(
            notes[0].detail.contains(&format!(
                "/run/media/u/das will not unmount: umount exited with code 32 after \
                 {UMOUNT_ATTEMPTS} attempts"
            )),
            "{notes:?}"
        );
        assert_eq!(
            argvs(&runner, "umount").len(),
            UMOUNT_ATTEMPTS as usize,
            "{:?}",
            runner.calls()
        );
        assert_eq!(
            runner.pauses.lock().unwrap().len(),
            UMOUNT_ATTEMPTS as usize - 1
        );
        assert!(
            !runner.calls().iter().any(|c| c.argv[0] == "mount"),
            "mounted beside it: {:?}",
            runner.calls()
        );
    }

    /// The stop is said as a warning when something was stopped, and not
    /// when nothing was; the report notes follow the same line.
    #[test]
    fn what_a_clearing_stopped_is_said_and_noted_only_when_something_was() {
        let scratch = Scratch::new();
        let (config, host, runner, _mnt) =
            raid1_mounted_by_udisks(&scratch, reader_at(300, "/run/media/u/das/a"));
        let progress = Recorder::default();
        let guard = host.mount_targets(&config, &progress, &runner).unwrap();
        assert!(
            logged(&progress, &format!("{STOPPED}: SIGTERM to pid 300")),
            "{:?}",
            progress.logs()
        );
        let notes = crate::backup::Release::clear_notes(&guard);
        assert_eq!(notes, guard.clear_notes().to_vec());
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(notes[0].outcome, ClearOutcome::Stopped);

        let scratch = Scratch::new();
        let (config, host, runner, _mnt) = raid1_mounted_by_udisks(&scratch, idle(300));
        let progress = Recorder::default();
        let guard = host.mount_targets(&config, &progress, &runner).unwrap();
        assert!(!logged(&progress, STOPPED), "{:?}", progress.logs());
        assert!(guard.clear_notes().is_empty(), "{:?}", guard.clear_notes());
        assert!(crate::backup::Release::clear_notes(&guard).is_empty());
        assert_eq!(guard.count(), 1);
    }
}
