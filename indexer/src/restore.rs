use crate::progress::{LogLevel, ProgressCallback};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Instant, UNIX_EPOCH};
use walkdir::WalkDir;

/// Result of a restore operation.
#[derive(Debug)]
pub struct RestoreResult {
    pub files_restored: usize,
    pub bytes_restored: u64,
    pub errors: Vec<String>,
    pub duration_secs: u64,
}

/// An entry returned by snapshot browsing.
#[derive(Debug, Clone)]
pub struct BrowseEntry {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
}

/// Browse files in a snapshot directory, returning entries matching an optional path prefix.
pub fn browse_snapshot(
    snapshot_path: &Path,
    prefix: Option<&str>,
) -> Result<Vec<BrowseEntry>, Box<dyn std::error::Error>> {
    let browse_dir = match prefix {
        Some(p) => snapshot_path.join(p),
        None => snapshot_path.to_path_buf(),
    };

    let mut entries: Vec<BrowseEntry> = Vec::new();

    for entry in std::fs::read_dir(&browse_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        // Use symlink_metadata so we don't follow symlinks for size/mtime
        let metadata = entry.metadata()?;

        let size = if file_type.is_dir() {
            0
        } else {
            metadata.len()
        };

        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let is_dir = file_type.is_dir();

        // Path relative to snapshot_path
        let abs_path = entry.path();
        let rel_path = abs_path
            .strip_prefix(snapshot_path)
            .unwrap_or(&abs_path)
            .to_string_lossy()
            .into_owned();

        let name = entry.file_name().to_string_lossy().into_owned();

        entries.push(BrowseEntry {
            path: rel_path,
            name,
            size,
            mtime,
            is_dir,
        });
    }

    // Sort: directories first, then alphabetically by name (case-insensitive)
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(entries)
}

/// Restore specific files from a snapshot to a destination.
/// Resolve `path` as far as it exists on disk, then re-attach the part that
/// does not exist yet.
///
/// A restore destination is routinely a directory that has not been created,
/// so plain `canonicalize()` would fail on exactly the input we most need to
/// judge. Resolving the existing prefix is what makes a symlinked ancestor
/// impossible to hide behind.
fn resolve_for_policy(path: &Path) -> std::io::Result<std::path::PathBuf> {
    let mut existing = path;
    let mut trailing = Vec::new();
    loop {
        if existing.exists() {
            let mut out = existing.canonicalize()?;
            for part in trailing.iter().rev() {
                out.push(part);
            }
            return Ok(out);
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                trailing.push(name.to_os_string());
                existing = parent;
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no existing ancestor of {}", path.display()),
                ));
            }
        }
    }
}

/// Refuse a restore destination that policy does not permit.
///
/// Two checks, denylist first: [`RESTORE_DENIED_ROOTS`] can never be written to
/// even if someone lists one in `allowed_roots`, and the destination must then
/// fall under one of the configured roots. Both are applied to the RESOLVED
/// path, so a symlinked ancestor cannot smuggle a write past them.
///
/// Before 0.7.20.0 there was no check at all: `restore_files` runs as root under
/// the D-Bus helper and would happily write into a systemd unit directory or a
/// package-manager hook directory (bd DAS-Backup-Manager-s05).
pub fn check_dest_allowed(
    dest: &Path,
    allowed_roots: &[String],
) -> Result<std::path::PathBuf, String> {
    let resolved = resolve_for_policy(dest)
        .map_err(|e| format!("Cannot resolve destination '{}': {e}", dest.display()))?;

    for denied in crate::config::RESTORE_DENIED_ROOTS {
        if resolved.starts_with(denied) {
            return Err(format!(
                "Refusing to restore into '{}': '{denied}' is never a permitted destination",
                resolved.display()
            ));
        }
    }

    if allowed_roots
        .iter()
        .any(|root| resolved.starts_with(Path::new(root)))
    {
        Ok(resolved)
    } else {
        Err(format!(
            "Refusing to restore into '{}': not under any [restore] allowed_roots ({})",
            resolved.display(),
            allowed_roots.join(", ")
        ))
    }
}

/// The directory roots a restore is permitted to read a snapshot FROM.
///
/// Both the configured mount point and whatever the target is *actually*
/// mounted at, because udisks2 mounts the same filesystem at
/// `/run/media/<user>/<label>` while the config names `/mnt/<label>`, and a
/// snapshot path recorded in the index can legitimately be either.
pub fn snapshot_source_roots(config: &crate::config::Config) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    for t in &config.targets {
        if !t.mount.is_empty() && !roots.contains(&t.mount) {
            roots.push(t.mount.clone());
        }
        if let Some(actual) = crate::health::find_any_mount(&t.mount, &t.serial, &t.role)
            && !roots.contains(&actual)
        {
            roots.push(actual);
        }
    }
    roots
}

/// Refuse to read a restore SOURCE that is not inside a configured backup target.
///
/// `check_dest_allowed` constrains where a restore may write. Nothing
/// constrained where it may READ: `restore_snapshot` and `restore_files` run as
/// root under the D-Bus helper and took the snapshot path straight from the
/// caller, so an authorized client could have root copy any directory tree on
/// the host — `/etc`, `/root`, another user's home — into a permitted
/// destination it can then read unprivileged (bd DAS-Backup-Manager-7ra).
///
/// Fails closed: an empty root list permits nothing, and the source must exist
/// (a restore from a non-existent snapshot has nothing to do anyway), so the
/// path is fully resolved and a symlinked ancestor cannot smuggle a read past
/// the check.
pub fn check_source_allowed(
    snapshot: &Path,
    source_roots: &[String],
) -> Result<std::path::PathBuf, String> {
    let resolved = snapshot
        .canonicalize()
        .map_err(|e| format!("Cannot resolve snapshot '{}': {e}", snapshot.display()))?;

    if source_roots.is_empty() {
        return Err(format!(
            "Refusing to restore from '{}': no backup targets are configured",
            resolved.display()
        ));
    }

    let permitted = source_roots.iter().any(|root| {
        let root_resolved = Path::new(root)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(root).to_path_buf());
        resolved.starts_with(&root_resolved)
    });

    if permitted {
        Ok(resolved)
    } else {
        Err(format!(
            "Refusing to restore from '{}': not inside any configured backup target ({})",
            resolved.display(),
            source_roots.join(", ")
        ))
    }
}

/// Reject a member path that could escape the snapshot or the destination.
///
/// One check covers both directions, because the same string is joined onto
/// each: a `..` component in `file_path` would otherwise let `src` climb out of
/// the snapshot AND let `dest_file` climb out of the validated destination.
fn safe_member(file_path: &str) -> Result<&Path, String> {
    use std::path::Component;
    let p = Path::new(file_path);
    if p.is_absolute() {
        return Err(format!("'{file_path}' is absolute"));
    }
    for component in p.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(format!(
                "'{file_path}' contains a non-literal path component"
            ));
        }
    }
    Ok(p)
}

/// Copy `src` over `dest` without following a symlink at `dest`.
///
/// `std::fs::copy` opens the destination write-truncate, which FOLLOWS a
/// symlink sitting at that path — so a pre-planted link at the destination
/// turned a restore into an overwrite of whatever it pointed at. `O_NOFOLLOW`
/// applies to the final component, which is precisely that case.
fn copy_no_follow(src: &Path, dest: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut reader = std::fs::File::open(src)?;
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dest)?;
    std::io::copy(&mut reader, &mut writer)
}

pub fn restore_files(
    snapshot_path: &Path,
    file_paths: &[&str],
    dest: &Path,
    allowed_roots: &[String],
    allowed_sources: &[String],
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let total = file_paths.len() as u64;

    // Destination policy is checked BEFORE anything is created — including the
    // destination directory itself, which `create_dir_all` below would
    // otherwise materialise inside a forbidden root.
    let dest = &check_dest_allowed(dest, allowed_roots)?;

    // Source policy is checked for the same reason and at the same point: the
    // snapshot path arrives from the caller and is read as root.
    let snapshot_root = check_source_allowed(snapshot_path, allowed_sources)?;
    let snapshot_path = snapshot_root.as_path();

    progress.on_stage("Restoring files", total);
    progress.on_log(
        LogLevel::Info,
        &format!("Restoring {} files to {}", total, dest.display()),
    );

    std::fs::create_dir_all(dest)?;

    let mut files_restored: usize = 0;
    let mut bytes_restored: u64 = 0;
    let mut errors: Vec<String> = Vec::new();

    for (i, file_path) in file_paths.iter().enumerate() {
        // Reject the member path itself. This is the check that actually stops
        // traversal, and unlike the old guard it CANNOT fail open: it inspects
        // the requested string, not whatever the filesystem happens to resolve.
        let member = match safe_member(file_path) {
            Ok(m) => m,
            Err(e) => {
                errors.push(format!("Path traversal blocked: {e}"));
                continue;
            }
        };
        let src = snapshot_path.join(member);
        let dest_file = dest.join(member);

        // Belt and braces: if `src` resolves at all, it must land inside the
        // snapshot. A resolve FAILURE is no longer silently tolerated for
        // regular files — only a symlink may legitimately fail to resolve, and
        // that case is handled explicitly below.
        if let Ok(canonical_src) = src.canonicalize()
            && !canonical_src.starts_with(&snapshot_root)
        {
            errors.push(format!("Path traversal blocked: '{}'", file_path));
            continue;
        }

        // Create parent directories for this file
        if let Some(parent) = dest_file.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            errors.push(format!("Failed to create dirs for '{}': {}", file_path, e));
            continue;
        }

        // Check if source is a symlink — preserve it as a symlink
        match std::fs::symlink_metadata(&src) {
            Err(e) => {
                errors.push(format!("Cannot stat '{}': {}", file_path, e));
                continue;
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                match std::fs::read_link(&src) {
                    Ok(target) => {
                        // Remove existing dest if present
                        let _ = std::fs::remove_file(&dest_file);
                        if let Err(e) = std::os::unix::fs::symlink(&target, &dest_file) {
                            errors.push(format!("Failed to create symlink '{}': {}", file_path, e));
                            continue;
                        }
                        bytes_restored += 0; // symlinks have no payload size
                        files_restored += 1;
                    }
                    Err(e) => {
                        errors.push(format!("Cannot read symlink '{}': {}", file_path, e));
                        continue;
                    }
                }
            }
            Ok(_) => match copy_no_follow(&src, &dest_file) {
                Ok(bytes) => {
                    bytes_restored += bytes;
                    files_restored += 1;
                }
                Err(e) => {
                    errors.push(format!("Failed to copy '{}': {}", file_path, e));
                    continue;
                }
            },
        }

        progress.on_progress(i as u64 + 1, total, file_path);
    }

    let duration_secs = start.elapsed().as_secs();
    let summary = format!(
        "Restored {}/{} files ({} bytes) in {}s, {} error(s)",
        files_restored,
        file_paths.len(),
        bytes_restored,
        duration_secs,
        errors.len()
    );

    progress.on_complete(errors.is_empty(), &summary);
    progress.on_log(LogLevel::Info, &summary);

    Ok(RestoreResult {
        files_restored,
        bytes_restored,
        errors,
        duration_secs,
    })
}

/// Restore an entire snapshot to a destination directory.
pub fn restore_snapshot(
    snapshot_path: &Path,
    dest: &Path,
    allowed_roots: &[String],
    allowed_sources: &[String],
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    let start = Instant::now();

    let dest = &check_dest_allowed(dest, allowed_roots)?;
    let snapshot_resolved = check_source_allowed(snapshot_path, allowed_sources)?;
    let snapshot_path = snapshot_resolved.as_path();

    progress.on_stage("Restoring snapshot", 1);
    progress.on_log(
        LogLevel::Info,
        &format!(
            "Restoring snapshot '{}' to '{}'",
            snapshot_path.display(),
            dest.display()
        ),
    );

    std::fs::create_dir_all(dest)?;

    // Attempt btrfs send | btrfs receive first (fast path for btrfs subvolumes)
    let btrfs_result = try_btrfs_send_receive(snapshot_path, dest, progress);

    match btrfs_result {
        Ok(result) => {
            let duration_secs = start.elapsed().as_secs();
            let summary = format!(
                "Snapshot restored via btrfs send/receive ({} bytes) in {}s",
                result.bytes_restored, duration_secs
            );
            progress.on_complete(result.errors.is_empty(), &summary);
            progress.on_log(LogLevel::Info, &summary);
            return Ok(RestoreResult {
                duration_secs,
                ..result
            });
        }
        Err(e) => {
            progress.on_log(
                LogLevel::Warning,
                &format!(
                    "btrfs send/receive not available ({}), falling back to recursive copy",
                    e
                ),
            );
        }
    }

    // Fallback: recursive copy preserving directory structure
    restore_snapshot_recursive(snapshot_path, dest, &start, progress)
}

/// Try to restore using `btrfs send | btrfs receive`.
fn try_btrfs_send_receive(
    snapshot_path: &Path,
    dest: &Path,
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    let mut send_child = Command::new("btrfs")
        .args(["send", &snapshot_path.to_string_lossy()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let send_stdout = send_child
        .stdout
        .take()
        .expect("btrfs send stdout pipe must be present");

    let mut recv_child = Command::new("btrfs")
        .args(["receive", &dest.to_string_lossy()])
        .stdin(send_stdout)
        .stderr(Stdio::null())
        .spawn()?;

    let send_status = send_child.wait()?;
    let recv_status = recv_child.wait()?;

    if !send_status.success() {
        return Err(format!("btrfs send exited with {}", send_status).into());
    }
    if !recv_status.success() {
        return Err(format!("btrfs receive exited with {}", recv_status).into());
    }

    progress.on_log(LogLevel::Info, "btrfs send/receive completed successfully");

    // Count bytes from the restored subvolume (best-effort)
    let bytes_restored = count_dir_bytes(dest);

    Ok(RestoreResult {
        files_restored: 0, // not easily countable via send/receive
        bytes_restored,
        errors: Vec::new(),
        duration_secs: 0, // caller fills in
    })
}

/// Fallback: recursive copy of the snapshot directory tree.
fn restore_snapshot_recursive(
    snapshot_path: &Path,
    dest: &Path,
    start: &Instant,
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    let mut files_restored: usize = 0;
    let mut bytes_restored: u64 = 0;
    let mut errors: Vec<String> = Vec::new();
    let mut file_index: u64 = 0;

    // Count total entries first for progress reporting (best-effort)
    let total_estimate: u64 = WalkDir::new(snapshot_path)
        .follow_links(false)
        .into_iter()
        .filter(|e| e.as_ref().map(|e| !e.file_type().is_dir()).unwrap_or(false))
        .count() as u64;

    for entry in WalkDir::new(snapshot_path).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errors.push(format!("WalkDir error: {}", e));
                continue;
            }
        };

        let rel_path = match entry.path().strip_prefix(snapshot_path) {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!(
                    "strip_prefix error for '{}': {}",
                    entry.path().display(),
                    e
                ));
                continue;
            }
        };

        // Skip the root itself
        if rel_path == Path::new("") {
            continue;
        }

        let dest_path = dest.join(rel_path);
        let file_type = entry.file_type();

        if file_type.is_dir() {
            if let Err(e) = std::fs::create_dir_all(&dest_path) {
                errors.push(format!(
                    "Failed to create dir '{}': {}",
                    dest_path.display(),
                    e
                ));
            }
            continue;
        }

        // Ensure parent dir exists
        if let Some(parent) = dest_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            errors.push(format!(
                "Failed to create parent dir '{}': {}",
                parent.display(),
                e
            ));
            continue;
        }

        if file_type.is_symlink() {
            match std::fs::read_link(entry.path()) {
                Ok(target) => {
                    let _ = std::fs::remove_file(&dest_path);
                    if let Err(e) = std::os::unix::fs::symlink(&target, &dest_path) {
                        errors.push(format!(
                            "Failed to create symlink '{}': {}",
                            dest_path.display(),
                            e
                        ));
                        continue;
                    }
                    files_restored += 1;
                    file_index += 1;
                    progress.on_progress(file_index, total_estimate, &rel_path.to_string_lossy());
                }
                Err(e) => {
                    errors.push(format!(
                        "Cannot read symlink '{}': {}",
                        entry.path().display(),
                        e
                    ));
                }
            }
            continue;
        }

        // Regular file.
        //
        // copy_no_follow, NOT std::fs::copy: the latter opens the destination
        // write-truncate, which FOLLOWS a symlink sitting at that path. That is
        // the bd s05 hazard, and this path had been left out of that fix —
        // restore_files used the guarded helper while restore_snapshot did not,
        // even though this is the COMMON path (try_btrfs_send_receive falls back
        // here for any snapshot that is not a receivable read-only subvolume).
        // The D-Bus RestoreSnapshot method runs as root and allows /tmp by
        // default, so a pre-planted link was enough to write snapshot content
        // through it and defeat RESTORE_DENIED_ROOTS entirely.
        // bd DAS-Backup-Manager-nsp (finding #3).
        match copy_no_follow(entry.path(), &dest_path) {
            Ok(bytes) => {
                bytes_restored += bytes;
                files_restored += 1;
                file_index += 1;
                progress.on_progress(file_index, total_estimate, &rel_path.to_string_lossy());
            }
            Err(e) => {
                errors.push(format!(
                    "Failed to copy '{}': {}",
                    entry.path().display(),
                    e
                ));
            }
        }
    }

    let duration_secs = start.elapsed().as_secs();
    let summary = format!(
        "Snapshot restored via recursive copy: {files_restored} files, {bytes_restored} bytes, {} error(s) in {duration_secs}s",
        errors.len()
    );

    progress.on_complete(errors.is_empty(), &summary);
    progress.on_log(LogLevel::Info, &summary);

    Ok(RestoreResult {
        files_restored,
        bytes_restored,
        errors,
        duration_secs,
    })
}

/// Sum the sizes of all regular files under a directory (best-effort, ignores errors).
fn count_dir_bytes(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::TestProgress;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: write a file with given content inside a base dir.
    // --- bd DAS-Backup-Manager-s05 --------------------------------------
    #[test]
    fn dest_policy_refuses_denied_roots_even_if_allowed() {
        // The denylist wins over configuration: listing a system root in
        // allowed_roots must NOT enable it.
        for denied in crate::config::RESTORE_DENIED_ROOTS {
            let err =
                check_dest_allowed(Path::new(denied), &[denied.to_string(), "/tmp".to_string()])
                    .expect_err("{denied} must never be a permitted destination");
            assert!(err.contains("never a permitted destination"), "{err}");
        }
    }

    #[test]
    fn dest_policy_admits_a_srv_subdir_while_still_refusing_the_srv_doc_roots() {
        // bd DAS-Backup-Manager-tku. Backing up /srv/VirtualMachines is useless
        // if it cannot be restored, so it is granted in allowed_roots — but as a
        // SUBDIRECTORY of /srv, never /srv itself, because /srv/http and
        // /srv/ftp are served to the network.
        check_dest_allowed(
            Path::new("/srv/VirtualMachines"),
            &["/srv/VirtualMachines".to_string()],
        )
        .expect("the granted VM subdirectory must be a permitted destination");

        // And the denylist holds even against the widest plausible mistake —
        // someone granting the whole of /srv later.
        for served in ["/srv/http", "/srv/ftp"] {
            let err = check_dest_allowed(Path::new(served), &["/srv".to_string()])
                .expect_err("a served document root must never be a destination");
            assert!(err.contains("never a permitted destination"), "{err}");
        }

        // Component-wise, not string-prefix: a sibling that merely starts with
        // the same characters must not be caught by the denylist.
        check_dest_allowed(Path::new("/srv/http-archive"), &["/srv".to_string()])
            .expect("/srv/http-archive is not /srv/http");
    }

    #[test]
    fn dest_policy_refuses_paths_outside_allowed_roots() {
        let outside = std::env::temp_dir().join("das-restore-outside-test");
        let err = check_dest_allowed(&outside, &["/home".to_string()])
            .expect_err("a temp path is not under /home");
        assert!(err.contains("allowed_roots"), "{err}");
    }

    #[test]
    fn dest_policy_admits_a_path_under_an_allowed_root() {
        let dir = TempDir::new().unwrap();
        // Positive control: without this, the two refusals above would pass
        // even if the function refused everything unconditionally.
        check_dest_allowed(dir.path(), &test_roots()).expect("temp dir must be allowed");
    }

    #[test]
    fn member_paths_may_not_escape() {
        for bad in ["../etc/passwd", "a/../../b", "/absolute/path", ".."] {
            assert!(safe_member(bad).is_err(), "{bad} must be rejected");
        }
        // Positive control.
        assert!(safe_member("dir/file.txt").is_ok());
    }

    #[test]
    fn restore_refuses_a_traversing_member_without_writing() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "ok.txt", "fine");
        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["../escape.txt", "ok.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 1, "only the safe member restores");
        assert!(
            result.errors.iter().any(|e| e.contains("traversal")),
            "{:?}",
            result.errors
        );
        assert!(!dest.path().parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn copy_does_not_follow_a_symlink_planted_at_the_destination() {
        // The concrete attack: a pre-planted link at dest_file turned a restore
        // into an overwrite of whatever it pointed at. `std::fs::copy` follows
        // it; `copy_no_follow` must not.
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "payload.txt", "attacker content");
        let dest = TempDir::new().unwrap();

        let victim = dest.path().join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        std::os::unix::fs::symlink(&victim, dest.path().join("payload.txt")).unwrap();

        let err = copy_no_follow(
            &snap.path().join("payload.txt"),
            &dest.path().join("payload.txt"),
        )
        .expect_err("must refuse to write through a symlink");
        // ELOOP on stable Rust: ErrorKind::FilesystemLoop is still unstable, so
        // assert the raw errno rather than a nightly-only variant.
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "original",
            "the symlink target must be untouched"
        );
    }

    #[test]
    fn restore_snapshot_recursive_does_not_follow_a_planted_symlink() {
        // The isolated copy_no_follow test above passed for months while THIS
        // path still called std::fs::copy — restore_files used the guarded
        // helper and restore_snapshot did not. A guard is only as wide as the
        // paths that call it, so the guarantee has to be asserted at the level
        // an attacker actually reaches. bd DAS-Backup-Manager-nsp (finding #3).
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "payload.txt", "attacker content");
        let dest = TempDir::new().unwrap();

        let victim = dest.path().join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        std::os::unix::fs::symlink(&victim, dest.path().join("payload.txt")).unwrap();

        let result = restore_snapshot_recursive(
            snap.path(),
            dest.path(),
            &Instant::now(),
            &crate::progress::NullProgress,
        )
        .expect("the walk itself should complete");

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "original",
            "root must not write snapshot content through a pre-planted symlink"
        );
        assert!(
            !result.errors.is_empty(),
            "refusing to follow the link must be REPORTED, not silently skipped"
        );
    }

    /// A restore SOURCE outside every configured target must be refused, and
    /// nothing may be copied.
    ///
    /// Counter-test for bd DAS-Backup-Manager-7ra: `restore_files` runs as root
    /// under the D-Bus helper, so without this the snapshot argument was an
    /// arbitrary root-read — copy `/etc` into an allowed root, then read it
    /// unprivileged.
    #[test]
    fn source_policy_refuses_a_snapshot_outside_configured_targets() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "secret.txt", "sensitive");
        let dest = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let err = restore_files(
            snap.path(),
            &["secret.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(elsewhere.path()),
            &progress,
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("not inside any configured backup target"),
            "{err}"
        );
        assert!(
            !dest.path().join("secret.txt").exists(),
            "the refusal must happen before anything is copied"
        );
    }

    /// With no targets configured at all, nothing is a permitted source.
    #[test]
    fn source_policy_fails_closed_with_no_configured_targets() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "a");
        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let err = restore_snapshot(snap.path(), dest.path(), &test_roots(), &[], &progress)
            .unwrap_err()
            .to_string();

        assert!(err.contains("no backup targets are configured"), "{err}");
    }

    /// Positive control for the pair above: the very same call succeeds once the
    /// snapshot's own directory is a permitted source. Without this, the two
    /// refusal tests would still pass if the function refused everything.
    #[test]
    fn source_policy_permits_a_snapshot_inside_a_configured_target() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "a");
        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["a.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 1);
        assert!(dest.path().join("a.txt").exists());
    }

    /// Permit a specific snapshot directory as a restore SOURCE.
    fn test_sources(snap: &Path) -> Vec<String> {
        vec![snap.to_string_lossy().into_owned()]
    }

    /// Allow the platform temp root, wherever TMPDIR points.
    fn test_roots() -> Vec<String> {
        vec![
            std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir())
                .to_string_lossy()
                .into_owned(),
        ]
    }

    fn write_file(base: &Path, rel: &str, content: &str) {
        let path = base.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn browse_entry_is_clone() {
        let entry = BrowseEntry {
            path: "/home/user/file.txt".into(),
            name: "file.txt".into(),
            size: 1024,
            mtime: 1709000000,
            is_dir: false,
        };
        let cloned = entry.clone();
        assert_eq!(cloned.path, "/home/user/file.txt");
        assert_eq!(cloned.size, 1024);
        assert!(!cloned.is_dir);
    }

    #[test]
    fn test_browse_snapshot_lists_files() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "alpha.txt", "hello");
        write_file(snap.path(), "beta.txt", "world");
        fs::create_dir_all(snap.path().join("subdir")).unwrap();

        let entries = browse_snapshot(snap.path(), None).unwrap();

        // We should have subdir + alpha.txt + beta.txt = 3 entries
        assert_eq!(entries.len(), 3);

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha.txt"));
        assert!(names.contains(&"beta.txt"));
        assert!(names.contains(&"subdir"));
    }

    #[test]
    fn test_browse_snapshot_with_prefix() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "root.txt", "root");
        write_file(snap.path(), "inner/a.txt", "a");
        write_file(snap.path(), "inner/b.txt", "b");

        let entries = browse_snapshot(snap.path(), Some("inner")).unwrap();

        // Only inner/a.txt and inner/b.txt — not root.txt
        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
        assert!(!names.contains(&"root.txt"));
    }

    #[test]
    fn test_browse_snapshot_sorts_dirs_first() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "zebra.txt", "z");
        write_file(snap.path(), "alpha.txt", "a");
        fs::create_dir_all(snap.path().join("middle_dir")).unwrap();
        fs::create_dir_all(snap.path().join("aaa_dir")).unwrap();

        let entries = browse_snapshot(snap.path(), None).unwrap();

        // All directories must precede all files
        let mut saw_file = false;
        for entry in &entries {
            if !entry.is_dir {
                saw_file = true;
            }
            if saw_file && entry.is_dir {
                panic!(
                    "Directory '{}' appeared after a file in sorted output",
                    entry.name
                );
            }
        }

        // Within directories: aaa_dir before middle_dir
        let dir_names: Vec<&str> = entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(dir_names, vec!["aaa_dir", "middle_dir"]);

        // Within files: alpha.txt before zebra.txt
        let file_names: Vec<&str> = entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(file_names, vec!["alpha.txt", "zebra.txt"]);
    }

    #[test]
    fn test_restore_files_copies_correctly() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "hello.txt", "hello world");
        write_file(snap.path(), "data.bin", "binary data here");

        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["hello.txt", "data.bin"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 2);
        assert!(result.errors.is_empty());

        let restored_hello = fs::read_to_string(dest.path().join("hello.txt")).unwrap();
        assert_eq!(restored_hello, "hello world");

        let restored_data = fs::read_to_string(dest.path().join("data.bin")).unwrap();
        assert_eq!(restored_data, "binary data here");
    }

    #[test]
    fn test_restore_files_preserves_structure() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "docs/guide.txt", "guide content");
        write_file(snap.path(), "docs/nested/deep.txt", "deep content");
        write_file(snap.path(), "root.txt", "root content");

        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["docs/guide.txt", "docs/nested/deep.txt", "root.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 3);
        assert!(result.errors.is_empty());

        // Verify nested structure preserved
        assert!(dest.path().join("docs/guide.txt").exists());
        assert!(dest.path().join("docs/nested/deep.txt").exists());
        assert!(dest.path().join("root.txt").exists());

        let guide = fs::read_to_string(dest.path().join("docs/guide.txt")).unwrap();
        assert_eq!(guide, "guide content");

        let deep = fs::read_to_string(dest.path().join("docs/nested/deep.txt")).unwrap();
        assert_eq!(deep, "deep content");
    }

    #[test]
    fn test_restore_files_reports_progress() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "a");
        write_file(snap.path(), "b.txt", "b");
        write_file(snap.path(), "c.txt", "c");

        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["a.txt", "b.txt", "c.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 3);
        assert!(result.errors.is_empty());

        // Verify on_stage was called with the right total
        let stages = progress.stages.lock().unwrap();
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].0, "Restoring files");
        assert_eq!(stages[0].1, 3);

        // Verify on_complete was called with success=true
        let completed = progress.completed.lock().unwrap();
        let (success, _summary) = completed
            .as_ref()
            .expect("on_complete should have been called");
        assert!(*success, "Expected success=true but got false");

        // Verify at least one log message was emitted
        let logs = progress.logs.lock().unwrap();
        assert!(!logs.is_empty(), "Expected at least one log message");
    }
}
