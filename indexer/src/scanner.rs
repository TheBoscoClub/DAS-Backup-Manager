use std::os::unix::fs::MetadataExt;
use std::path::Path;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,   // relative path within snapshot
    pub name: String,   // basename
    pub size: i64,
    pub mtime: i64,
    pub file_type: i32, // 0=regular, 1=directory, 2=symlink, 3=other
}

pub struct ScanResult {
    pub entries: Vec<FileEntry>,
    pub errors: usize,
}

pub fn scan_directory(root: &Path) -> ScanResult {
    let mut entries = Vec::new();
    let mut errors = 0usize;

    for entry in WalkDir::new(root).min_depth(1) {
        match entry {
            Ok(e) => {
                let rel_path = e.path().strip_prefix(root).unwrap_or(e.path());
                let rel_str = rel_path.to_string_lossy().to_string();
                let name = e.file_name().to_string_lossy().to_string();

                let ft = e.file_type();
                let file_type = if ft.is_symlink() {
                    2
                } else if ft.is_dir() {
                    1
                } else if ft.is_file() {
                    0
                } else {
                    3
                };

                let (size, mtime) = match e.metadata() {
                    Ok(m) => (m.len() as i64, m.mtime()),
                    Err(_) => {
                        errors += 1;
                        (0, 0)
                    }
                };

                entries.push(FileEntry {
                    path: rel_str,
                    name,
                    size,
                    mtime,
                    file_type,
                });
            }
            Err(_) => {
                errors += 1;
            }
        }
    }

    ScanResult { entries, errors }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn walks_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let result = scan_directory(tmp.path());
        assert_eq!(result.entries.len(), 0);
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn walks_files_and_dirs() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        fs::write(tmp.path().join("b.txt"), "world").unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        fs::write(tmp.path().join("subdir/c.txt"), "nested").unwrap();
        let result = scan_directory(tmp.path());
        // a.txt, b.txt, subdir/, subdir/c.txt = 4 entries
        assert_eq!(result.entries.len(), 4);
    }

    #[test]
    fn captures_metadata() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "12345").unwrap();
        let result = scan_directory(tmp.path());
        let entry = result.entries.iter().find(|e| e.name == "test.txt").unwrap();
        assert_eq!(entry.size, 5);
        assert_eq!(entry.path, "test.txt");
        assert_eq!(entry.file_type, 0);
    }

    #[test]
    fn identifies_symlinks() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("real.txt"), "content").unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("real.txt"),
            tmp.path().join("link.txt"),
        )
        .unwrap();
        let result = scan_directory(tmp.path());
        let link = result.entries.iter().find(|e| e.name == "link.txt").unwrap();
        assert_eq!(link.file_type, 2);
    }

    #[test]
    fn relative_paths() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        fs::write(tmp.path().join("a/b/deep.txt"), "deep").unwrap();
        let result = scan_directory(tmp.path());
        let deep = result.entries.iter().find(|e| e.name == "deep.txt").unwrap();
        assert_eq!(deep.path, "a/b/deep.txt");
    }
}
