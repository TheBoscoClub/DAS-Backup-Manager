//! Auto-mount and unmount DAS backup targets.
//!
//! Provides [`ensure_targets_mounted`] which resolves target drive serials to
//! block devices, mounts any that are not already mounted, and returns a
//! [`MountGuard`] whose [`Drop`] implementation unmounts only the targets that
//! *this* call mounted — never interfering with mounts managed by the bash
//! scripts or by the user.

use std::fmt;
use std::path::Path;
use std::process::Command;

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
/// - **Mirror** and **EspSync** targets use the second partition (`{dev}2`) —
///   partition 1 is the ESP, partition 2 is the BTRFS data area.
pub fn partition_device(dev: &str, role: &TargetRole) -> String {
    match role {
        TargetRole::Primary => format!("{dev}1"),
        TargetRole::Mirror | TargetRole::EspSync => format!("{dev}2"),
    }
}

/// RAII guard that unmounts targets on drop.
///
/// Only the mount points that *this* guard mounted are tracked. Pre-existing
/// mounts (from bash scripts, manual mounts, or a previous guard) are never
/// touched.
pub struct MountGuard {
    newly_mounted: Vec<String>,
}

impl MountGuard {
    fn new() -> Self {
        Self {
            newly_mounted: Vec::new(),
        }
    }

    /// Explicitly unmount all targets this guard mounted, with progress
    /// reporting. Prefer calling this over relying on `Drop` so that unmount
    /// errors can be logged.
    pub fn unmount(&mut self, progress: &dyn ProgressCallback) {
        if self.newly_mounted.is_empty() {
            return;
        }
        progress.on_stage("Unmounting targets", self.newly_mounted.len() as u64);
        // Unmount in reverse order (LIFO)
        for (i, mount_point) in self.newly_mounted.drain(..).rev().enumerate() {
            let status = Command::new("umount").arg(&mount_point).status();
            match status {
                Ok(s) if s.success() => {
                    progress.on_progress((i + 1) as u64, 0, &format!("Unmounted {mount_point}"));
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
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        // Safety net: unmount anything not yet explicitly unmounted.
        for mount_point in self.newly_mounted.drain(..).rev() {
            let _ = Command::new("umount").arg(&mount_point).status();
        }
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
    let mut guard = MountGuard::new();
    let mut any_available = false;
    let total = config.targets.len() as u64;

    if total == 0 {
        return Ok(guard);
    }

    progress.on_stage("Mounting targets", total);

    for (i, target) in config.targets.iter().enumerate() {
        let mount_path = Path::new(&target.mount);

        // Already mounted at configured path — nothing to do
        if mount_path.exists() && health::is_mountpoint(mount_path) {
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
        if let Some(actual) = health::find_mount_for_device(&target.serial, &target.role) {
            // Create configured mount point directory if needed
            if !mount_path.exists() {
                let status = Command::new("mkdir").arg("-p").arg(&target.mount).status();
                if status.is_err() || !status.unwrap().success() {
                    progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!(
                            "Failed to create mount point {} for bind mount",
                            target.mount
                        ),
                    );
                    any_available = true;
                    continue;
                }
            }
            let status = Command::new("mount")
                .arg("--bind")
                .arg(&actual)
                .arg(&target.mount)
                .status();
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
                            "{}: bind mount {} → {} failed — btrbk may not find this target",
                            target.label, actual, target.mount
                        ),
                    );
                    any_available = true;
                }
            }
            continue;
        }

        // Resolve serial → /dev/sdX
        let dev = match health::device_from_serial(&target.serial) {
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
        if !mount_path.exists() {
            let mkdir = Command::new("mkdir").args(["-p", &target.mount]).status();
            match mkdir {
                Ok(s) if s.success() => {}
                _ => {
                    progress.on_log(
                        crate::progress::LogLevel::Warning,
                        &format!(
                            "Target '{}': mkdir -p '{}' failed — skipping",
                            target.label, target.mount
                        ),
                    );
                    continue;
                }
            }
        }

        // Build mount command
        let mut cmd = Command::new("mount");
        cmd.args(["-t", "btrfs"]);
        if !config.das.mount_opts.is_empty() {
            cmd.args(["-o", &config.das.mount_opts]);
        }
        cmd.arg(&part_dev).arg(&target.mount);

        let mount_result = cmd.output();
        match mount_result {
            Ok(output) if output.status.success() => {
                any_available = true;
                guard.newly_mounted.push(target.mount.clone());
                progress.on_progress(
                    (i + 1) as u64,
                    total,
                    &format!("Mounted {} at {}", target.label, target.mount),
                );
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
            }
            Err(e) => {
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!("Target '{}': failed to execute mount: {e}", target.label),
                );
            }
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
pub fn ensure_sources_mounted(
    config: &Config,
    progress: &dyn ProgressCallback,
) -> MountGuard {
    let mut guard = MountGuard::new();

    // Deduplicate: multiple sources can share a volume (e.g. hdd-projects
    // and hdd-audiobooks both use /.btrfs-hdd).
    let mut seen_volumes = std::collections::HashSet::new();

    for source in &config.sources {
        if !seen_volumes.insert(source.volume.clone()) {
            continue;
        }

        let mount_path = Path::new(&source.volume);

        // Already mounted — nothing to do.
        if mount_path.exists() && health::is_mountpoint(mount_path) {
            progress.on_log(
                crate::progress::LogLevel::Info,
                &format!("Source volume {} already mounted", source.volume),
            );
            continue;
        }

        // Create mount point if needed.
        if !mount_path.exists() {
            let status = Command::new("mkdir").args(["-p", &source.volume]).status();
            if status.is_err() || !status.unwrap().success() {
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!(
                        "Source '{}': mkdir -p '{}' failed — skipping",
                        source.label, source.volume
                    ),
                );
                continue;
            }
        }

        // Also create the snapshot directory inside the volume.
        // btrbk requires it to exist before creating snapshots.
        // We do this after mounting (below), but record it here for later.

        // Mount with subvolid=5 to expose the top-level BTRFS tree.
        let mount_result = Command::new("mount")
            .args(["-o", "subvolid=5", &source.device, &source.volume])
            .output();

        match mount_result {
            Ok(output) if output.status.success() => {
                guard.newly_mounted.push(source.volume.clone());
                progress.on_log(
                    crate::progress::LogLevel::Info,
                    &format!(
                        "Source '{}': mounted {} at {} (subvolid=5)",
                        source.label, source.device, source.volume
                    ),
                );
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
            }
            Err(e) => {
                progress.on_log(
                    crate::progress::LogLevel::Warning,
                    &format!(
                        "Source '{}': failed to execute mount: {e}",
                        source.label
                    ),
                );
            }
        }
    }

    // Create snapshot directories inside now-mounted source volumes.
    for source in &config.sources {
        let snap_dir = Path::new(&source.volume).join(&source.snapshot_dir);
        if !snap_dir.exists() {
            let _ = Command::new("mkdir").args(["-p"]).arg(&snap_dir).status();
        }
    }

    // Create target subdirectories on mounted targets (btrbk expects them).
    for target in &config.targets {
        let target_path = Path::new(&target.mount);
        if !target_path.exists() || !health::is_mountpoint(target_path) {
            continue;
        }
        for source in &config.sources {
            for subdir in &source.target_subdirs {
                let dir = target_path.join(subdir);
                if !dir.exists() {
                    let _ = Command::new("mkdir").args(["-p"]).arg(&dir).status();
                }
            }
        }
    }

    guard
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn partition_device_esp_sync() {
        assert_eq!(
            partition_device("/dev/sdd", &TargetRole::EspSync),
            "/dev/sdd2"
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
        let guard = MountGuard::new();
        assert_eq!(guard.count(), 0);
    }

    #[test]
    fn mount_guard_tracks_mounts() {
        let mut guard = MountGuard::new();
        guard.newly_mounted.push("/mnt/test1".into());
        guard.newly_mounted.push("/mnt/test2".into());
        assert_eq!(guard.count(), 2);
        // Drop will try to unmount, which will fail silently (paths don't exist)
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
