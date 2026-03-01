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
pub fn restore_files(
    snapshot_path: &Path,
    file_paths: &[&str],
    dest: &Path,
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let total = file_paths.len() as u64;

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
        let src = snapshot_path.join(file_path);
        let dest_file = dest.join(file_path);

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
            Ok(_) => match std::fs::copy(&src, &dest_file) {
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
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    let start = Instant::now();

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

        // Regular file
        match std::fs::copy(entry.path(), &dest_path) {
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
