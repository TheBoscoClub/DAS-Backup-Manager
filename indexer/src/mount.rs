//! Auto-mount and unmount DAS backup targets.
//!
//! Provides [`ensure_targets_mounted`] which resolves target drive serials to
//! block devices, mounts any that are not already mounted, and returns a
//! [`MountGuard`] whose [`Drop`] implementation unmounts only the targets that
//! *this* call mounted — never interfering with mounts managed by the bash
//! scripts or by the user.

use std::fmt;
use std::io;
use std::panic::RefUnwindSafe;
use std::path::Path;
use std::process::{Command, ExitStatus, Output};
use std::sync::Arc;

use crate::config::{Config, TargetRole};
use crate::health;
use crate::progress::ProgressCallback;

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
}

impl fmt::Display for MountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDrivesFound => write!(f, "No DAS drives found — is the enclosure powered on?"),
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
}

/// The questions this module asks about the host's mount table and
/// `/dev/disk/by-id`. Held as borrowed functions rather than called directly
/// so the mount logic can be run against a described host; production code
/// only ever uses [`HOST_PROBES`].
struct MountProbes<'a> {
    is_mountpoint: &'a dyn Fn(&Path) -> bool,
    find_mount_for_device: &'a dyn Fn(&str, &TargetRole) -> Option<String>,
    device_from_serial: &'a dyn Fn(&str) -> Option<String>,
}

const HOST_PROBES: MountProbes<'static> = MountProbes {
    is_mountpoint: &health::is_mountpoint,
    find_mount_for_device: &health::find_mount_for_device,
    device_from_serial: &health::device_from_serial,
};

/// RAII guard that unmounts targets on drop.
///
/// Only the mount points that *this* guard mounted are tracked. Pre-existing
/// mounts (from bash scripts, manual mounts, or a previous guard) are never
/// touched.
pub struct MountGuard {
    newly_mounted: Vec<String>,
    runner: Arc<dyn CommandRunner>,
}

impl MountGuard {
    fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            newly_mounted: Vec::new(),
            runner,
        }
    }

    /// Explicitly unmount all targets this guard mounted, with progress
    /// reporting. Prefer calling this over relying on `Drop` so that unmount
    /// errors can be logged.
    pub fn unmount(&mut self, progress: &dyn ProgressCallback) {
        if self.newly_mounted.is_empty() {
            return;
        }
        let total = self.newly_mounted.len() as u64;
        progress.on_stage("Unmounting targets", total);
        // Unmount in reverse order (LIFO)
        for (i, mount_point) in self.newly_mounted.drain(..).rev().enumerate() {
            let status = self.runner.status(Command::new("umount").arg(&mount_point));
            match status {
                Ok(s) if s.success() => {
                    progress.on_progress(
                        (i + 1) as u64,
                        total,
                        &format!("Unmounted {mount_point}"),
                    );
                }
                Ok(s) => {
                    progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!(
                            "umount {mount_point} exited with code {}",
                            s.code().unwrap_or(-1)
                        ),
                    );
                }
                Err(e) => {
                    progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!("umount {mount_point} failed: {e}"),
                    );
                }
            }
        }
    }

    /// How many mount points this guard is responsible for.
    pub fn count(&self) -> usize {
        self.newly_mounted.len()
    }

    /// Unmount everything still held, in reverse order, and return one message
    /// per mount point that could not be unmounted. `Drop` has nowhere to send
    /// these but stderr; they are returned rather than printed here so that
    /// "a failed unmount is reported, a clean one is not" can be tested.
    fn unmount_remaining(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        for mount_point in self.newly_mounted.drain(..).rev() {
            match self.runner.status(Command::new("umount").arg(&mount_point)) {
                Ok(s) if s.success() => {}
                Ok(s) => failures.push(format!(
                    "MountGuard::drop: umount {mount_point} exited {} — target left mounted",
                    s.code().unwrap_or(-1)
                )),
                Err(e) => failures.push(format!(
                    "MountGuard::drop: umount {mount_point} failed to run: {e}"
                )),
            }
        }
        failures
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
/// 2. Resolve the serial number to a block device via `/dev/disk/by-id/`.
/// 3. Determine the partition device based on target role.
/// 4. `mkdir -p` the mount point, then `mount -t btrfs -o <opts>`.
/// 5. Track newly-mounted targets in the returned [`MountGuard`].
///
/// Returns `Err(MountError::NoDrivesFound)` only if **no** targets could be
/// mounted *and* no targets were already mounted. Individual mount failures
/// are logged as warnings but do not abort the operation.
pub fn ensure_targets_mounted(
    config: &Config,
    progress: &dyn ProgressCallback,
) -> Result<MountGuard, MountError> {
    ensure_targets_mounted_with(config, progress, &HOST_PROBES, Arc::new(SystemRunner))
}

/// [`ensure_targets_mounted`] against an explicit host: `probes` answers what
/// is mounted and where the drives are, `runner` executes `mount`.
fn ensure_targets_mounted_with(
    config: &Config,
    progress: &dyn ProgressCallback,
    probes: &MountProbes<'_>,
    runner: Arc<dyn CommandRunner>,
) -> Result<MountGuard, MountError> {
    let mut guard = MountGuard::new(runner);
    let mut any_available = false;
    let total = config.targets.len() as u64;

    if total == 0 {
        return Ok(guard);
    }

    progress.on_stage("Mounting targets", total);

    for (i, target) in config.targets.iter().enumerate() {
        let mount_path = Path::new(&target.mount);

        // Already mounted at configured path — nothing to do
        if mount_path.exists() && (probes.is_mountpoint)(mount_path) {
            any_available = true;
            progress.on_progress(
                (i + 1) as u64,
                total,
                &format!("{} already mounted", target.label),
            );
            continue;
        }

        // Already mounted elsewhere (e.g. udisks2 at /run/media/) — bind-mount
        // at the configured path so btrbk can find the target where it expects it.
        if let Some(actual) = (probes.find_mount_for_device)(&target.serial, &target.role) {
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

        // Resolve serial → /dev/sdX
        let dev = match (probes.device_from_serial)(&target.serial) {
            Some(d) => d,
            None => {
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!(
                        "Target '{}': drive serial '{}' not found — skipping",
                        target.label, target.serial
                    ),
                );
                continue;
            }
        };

        // Determine partition device
        let part_dev = partition_device(&dev, &target.role);
        if !Path::new(&part_dev).exists() {
            progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!(
                    "Target '{}': partition '{}' not found — skipping",
                    target.label, part_dev
                ),
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

        // Build mount command
        let mut cmd = Command::new("mount");
        cmd.args(["-t", "btrfs"]);
        if !config.das.mount_opts.is_empty() {
            cmd.args(["-o", &config.das.mount_opts]);
        }
        cmd.arg(&part_dev).arg(&target.mount);

        let mount_result = guard.runner.output(&mut cmd);
        let mounted = match mount_result {
            Ok(output) if output.status.success() => {
                any_available = true;
                guard.newly_mounted.push(target.mount.clone());
                progress.on_progress(
                    (i + 1) as u64,
                    total,
                    &format!("Mounted {} at {}", target.label, target.mount),
                );
                true
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!(
                        "Target '{}': mount {} → {} failed: {}",
                        target.label,
                        part_dev,
                        target.mount,
                        stderr.trim()
                    ),
                );
                false
            }
            Err(e) => {
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!("Target '{}': failed to execute mount: {e}", target.label),
                );
                false
            }
        };
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
    let mut guard = MountGuard::new(runner);

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
            // No UUID configured (legacy target). The mount-point check above
            // is all the assurance available; say so rather than implying more.
            progress.on_log(
                crate::progress::LogLevel::Warning,
                &format!(
                    "Target '{}' has no mount_uuid configured — verified only that '{}' \
                     is a mount point, not which filesystem it is",
                    target.label, target.mount
                ),
            );
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
    #[derive(Clone)]
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
            })
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn run(&self, captured: bool, cmd: &Command) -> io::Result<Output> {
            let mut argv = vec![cmd.get_program().to_string_lossy().into_owned()];
            argv.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
            let reply = self
                .rules
                .iter()
                .find(|(key, _)| argv.iter().any(|a| a == key))
                .map(|(_, reply)| reply.clone())
                .unwrap_or_else(Reply::ok);
            if matches!(reply, Reply::Exit { code: 0, .. })
                && let Some(mount_point) = argv.last()
            {
                let mut mounted = self.mounted.lock().unwrap();
                match argv[0].as_str() {
                    "mount" => mounted.push(mount_point.clone()),
                    "umount" => mounted.retain(|m| m != mount_point),
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
                Reply::CannotRun => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "scripted: no such program",
                )),
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
    }

    impl Host {
        fn with_probes<R>(
            &self,
            runner: &ScriptedRunner,
            f: impl FnOnce(&MountProbes<'_>) -> R,
        ) -> R {
            let is_mountpoint = |p: &Path| {
                self.mountpoints.iter().any(|m| m == p)
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
            f(&MountProbes {
                is_mountpoint: &is_mountpoint,
                find_mount_for_device: &find_mount_for_device,
                device_from_serial: &device_from_serial,
            })
        }

        fn mount_targets(
            &self,
            config: &Config,
            progress: &Recorder,
            runner: &Arc<ScriptedRunner>,
        ) -> Result<MountGuard, MountError> {
            self.with_probes(runner, |p| {
                ensure_targets_mounted_with(config, progress, p, runner.clone())
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

    /// `TestProgress` drops `on_progress`; the step numbers and messages are
    /// part of what these functions report, so record them too.
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

        guard.unmount(&progress);

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

    /// A umount that fails is a warning, never a progress step: reporting
    /// "Unmounted X" for a target that is still mounted is how the next run's
    /// mount-point reasoning goes wrong.
    #[test]
    fn unmount_warns_about_each_mount_point_it_could_not_release() {
        let runner = ScriptedRunner::with_rules(&[
            ("/t/busy", Reply::exit(32)),
            ("/t/norun", Reply::CannotRun),
        ]);
        let progress = Recorder::default();
        let mut guard = guard_holding(&runner, &["/t/busy", "/t/norun"]);

        guard.unmount(&progress);

        assert_eq!(runner.calls().len(), 2);
        assert!(progress.steps().is_empty(), "{:?}", progress.steps());
        let logs = progress.logs();
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert_eq!(logs[0].0, LogLevel::Warning);
        assert!(logs[0].1.contains("umount /t/norun failed:"), "{logs:?}");
        assert_eq!(
            logs[1],
            (
                LogLevel::Warning,
                "umount /t/busy exited with code 32".to_string()
            )
        );
    }

    #[test]
    fn unmount_on_an_empty_guard_does_nothing() {
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();
        let mut guard = MountGuard::new(runner.clone());

        guard.unmount(&progress);
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
    /// point left behind.
    #[test]
    fn unmount_remaining_reports_only_the_failures() {
        let runner = ScriptedRunner::with_rules(&[
            ("/t/busy", Reply::exit(32)),
            ("/t/norun", Reply::CannotRun),
        ]);
        let mut guard = guard_holding(&runner, &["/t/busy", "/t/fine", "/t/norun"]);

        let failures = guard.unmount_remaining();

        assert_eq!(runner.calls().len(), 3);
        assert_eq!(guard.count(), 0);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(
            failures[0].starts_with("MountGuard::drop: umount /t/norun failed to run:"),
            "{failures:?}"
        );
        assert_eq!(
            failures[1],
            "MountGuard::drop: umount /t/busy exited 32 — target left mounted"
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

    /// Targets already mounted where they belong are available, are reported,
    /// and are NOT the guard's to unmount.
    #[test]
    fn targets_already_mounted_are_reported_and_left_alone() {
        let scratch = Scratch::new();
        let a = scratch.mkdir("mnt/a");
        let b = scratch.mkdir("mnt/b");
        let config = config_with(
            vec![
                target("alpha", "SER-A", &a, TargetRole::Primary),
                target("beta", "SER-B", &b, TargetRole::Mirror),
            ],
            Vec::new(),
        );
        let host = Host {
            mountpoints: vec![PathBuf::from(&a), PathBuf::from(&b)],
            ..Host::default()
        };
        let runner = ScriptedRunner::succeeding();
        let progress = Recorder::default();

        let guard = host.mount_targets(&config, &progress, &runner).unwrap();

        assert_eq!(guard.count(), 0);
        drop(guard);
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert_eq!(progress.stages(), vec![("Mounting targets".into(), 2)]);
        assert_eq!(
            progress.steps(),
            vec![
                (1, 2, "alpha already mounted".to_string()),
                (2, 2, "beta already mounted".to_string()),
            ]
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

    #[test]
    fn filesystem_uuid_is_read_from_findmnt_and_absent_on_any_failure() {
        let uuid = "b2dbe07d-40b9-422e-8ccf-ef4931c40457";
        let found: &'static str = "b2dbe07d-40b9-422e-8ccf-ef4931c40457\n";
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

    const EXPECTED_UUID: &str = "b2dbe07d-40b9-422e-8ccf-ef4931c40457";
    const EXPECTED_UUID_LINE: &str = "b2dbe07d-40b9-422e-8ccf-ef4931c40457\n";

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
                stdout: "60b05268-0000-0000-0000-000000000000\n",
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
                 '60b05268-0000-0000-0000-000000000000', expected '{EXPECTED_UUID}'"
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

    /// No (or an empty) `mount_uuid` is a legacy target: accepted on the
    /// mount-point check alone, with a warning that says that is all it was.
    #[test]
    fn verify_warns_when_a_write_target_has_no_uuid_to_check() {
        for uuid in [None, Some("")] {
            let scratch = Scratch::new();
            let (targets, host, mnt) = mounted_write_target(&scratch, uuid);
            let runner = ScriptedRunner::succeeding();
            let progress = Recorder::default();

            let result = host.verify(&targets, &["primary-22tb"], &progress, &runner);

            assert_eq!(result, Ok(()));
            assert!(runner.calls().is_empty(), "{:?}", runner.calls());
            progress.assert_only_log(
                LogLevel::Warning,
                &["Target 'primary-22tb' has no mount_uuid configured", &mnt],
            );
        }
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

    /// Positive control: a real mount point passes. Without this, the refusal
    /// test above would still pass if the function refused everything.
    #[test]
    fn verify_accepts_a_real_mountpoint_write_target() {
        // /proc is a mount point in every Linux test environment.
        let targets = vec![target_at("primary-22tb", "/proc", None)];
        let progress = TestProgress::new();

        assert!(verify_write_targets(&targets, &["primary-22tb".to_string()], &progress).is_ok());
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
        let result = ensure_targets_mounted(&config, &progress);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().count(), 0);
    }
}
