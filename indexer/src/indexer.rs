use regex::Regex;
use std::fs;
use std::path::PathBuf;

use crate::db::Database;

#[derive(Debug, Clone)]
pub struct DiscoveredSnapshot {
    pub name: String,   // "root"
    pub ts: String,     // "20260221T0304"
    pub source: String, // "nvme"
    pub path: PathBuf,  // full path to snapshot dir
}

/// Parse a btrbk snapshot directory name like `root.20260221T0304` into (name, timestamp).
/// Returns `None` if the dirname doesn't match the expected pattern.
pub fn parse_snapshot_dirname(dirname: &str) -> Option<(String, String)> {
    let re = Regex::new(r"^(.+)\.(\d{8}T\d{4,6})$").unwrap();
    re.captures(dirname)
        .map(|caps| (caps[1].to_string(), caps[2].to_string()))
}

/// Walk a backup target directory, find snapshot directories matching the btrbk
/// naming convention `<name>.<YYYYMMDDTHHMMSS>`, and skip any that are already
/// indexed in the database.
///
/// Expected directory structure:
/// ```text
/// target_root/
///   nvme/                    (source directory)
///     root.20260221T0304/    (snapshot)
///     home.20260221T0304/
///   ssd/                     (source directory)
///     opt.20260221T0304/
/// ```
pub fn discover_snapshots(
    target_root: &std::path::Path,
    db: &Database,
) -> Result<Vec<DiscoveredSnapshot>, Box<dyn std::error::Error>> {
    let mut discovered = Vec::new();

    for source_entry in fs::read_dir(target_root)? {
        let source_entry = source_entry?;
        if !source_entry.file_type()?.is_dir() {
            continue;
        }
        let source = source_entry.file_name().to_string_lossy().to_string();

        for snap_entry in fs::read_dir(source_entry.path())? {
            let snap_entry = snap_entry?;
            if !snap_entry.file_type()?.is_dir() {
                continue;
            }
            let dirname = snap_entry.file_name().to_string_lossy().to_string();
            if let Some((name, ts)) = parse_snapshot_dirname(&dirname) {
                let path = snap_entry.path();
                let path_str = path.to_string_lossy().to_string();
                if !db.snapshot_exists(&path_str)? {
                    discovered.push(DiscoveredSnapshot {
                        name,
                        ts,
                        source: source.clone(),
                        path,
                    });
                }
            }
        }
    }

    discovered.sort_by(|a, b| (&a.source, &a.name, &a.ts).cmp(&(&b.source, &b.name, &b.ts)));
    Ok(discovered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_snap_dirs(tmp: &TempDir, dirs: &[&str]) {
        for d in dirs {
            fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
    }

    #[test]
    fn discovers_snapshots() {
        let tmp = TempDir::new().unwrap();
        make_snap_dirs(&tmp, &["nvme/root.20260220T0300", "nvme/root.20260221T0300"]);
        let db = Database::open(":memory:").unwrap();
        let snaps = discover_snapshots(tmp.path(), &db).unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].name, "root");
        assert_eq!(snaps[0].source, "nvme");
    }

    #[test]
    fn parses_snapshot_name() {
        let (name, ts) = parse_snapshot_dirname("root.20260221T0304").unwrap();
        assert_eq!(name, "root");
        assert_eq!(ts, "20260221T0304");
    }

    #[test]
    fn parses_compound_name() {
        let (name, ts) = parse_snapshot_dirname("root-home.20260221T0304").unwrap();
        assert_eq!(name, "root-home");
        assert_eq!(ts, "20260221T0304");
    }

    #[test]
    fn skips_already_indexed() {
        let tmp = TempDir::new().unwrap();
        make_snap_dirs(&tmp, &["nvme/root.20260220T0300", "nvme/root.20260221T0300"]);
        let db = Database::open(":memory:").unwrap();
        let path1 = tmp.path().join("nvme/root.20260220T0300");
        db.insert_snapshot("root", "20260220T0300", "nvme", &path1.to_string_lossy())
            .unwrap();
        let snaps = discover_snapshots(tmp.path(), &db).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].ts, "20260221T0300");
    }

    #[test]
    fn discovers_sources() {
        let tmp = TempDir::new().unwrap();
        make_snap_dirs(
            &tmp,
            &[
                "nvme/root.20260221T0300",
                "ssd/opt.20260221T0300",
                "projects/claude-projects.20260221T0300",
            ],
        );
        let db = Database::open(":memory:").unwrap();
        let snaps = discover_snapshots(tmp.path(), &db).unwrap();
        let sources: Vec<&str> = snaps.iter().map(|s| s.source.as_str()).collect();
        assert!(sources.contains(&"nvme"));
        assert!(sources.contains(&"ssd"));
        assert!(sources.contains(&"projects"));
    }
}
