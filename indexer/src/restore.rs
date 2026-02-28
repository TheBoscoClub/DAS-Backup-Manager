use crate::progress::ProgressCallback;
use std::path::Path;

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

/// Restore specific files from a snapshot to a destination.
pub fn restore_files(
    _snapshot_path: &Path,
    _file_paths: &[&str],
    _dest: &Path,
    _progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

/// Restore an entire snapshot to a destination directory.
pub fn restore_snapshot(
    _snapshot_path: &Path,
    _dest: &Path,
    _progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

/// Browse files in a snapshot directory, returning entries matching an optional path prefix.
pub fn browse_snapshot(
    _snapshot_path: &Path,
    _prefix: Option<&str>,
) -> Result<Vec<BrowseEntry>, Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
