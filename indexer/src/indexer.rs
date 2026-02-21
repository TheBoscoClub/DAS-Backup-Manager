use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::db::Database;
use crate::scanner::scan_directory;

#[derive(Debug, Clone)]
pub struct DiscoveredSnapshot {
    pub name: String,   // "root"
    pub ts: String,     // "20260221T0304"
    pub source: String, // "nvme"
    pub path: PathBuf,  // full path to snapshot dir
}

#[derive(Debug)]
pub struct WalkResult {
    pub snapshots_discovered: usize,
    pub snapshots_indexed: usize,
    pub snapshots_skipped: usize,
    pub results: Vec<IndexResult>,
}

#[derive(Debug)]
pub struct IndexResult {
    pub snapshot_id: i64,
    pub files_total: usize,
    pub files_new: usize,
    pub files_extended: usize,
    pub files_changed: usize,
    pub scan_errors: usize,
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

/// Index a single snapshot directory into the database.
///
/// Walks the snapshot's filesystem, upserts each file into the `files` table,
/// and creates or extends spans. If `prev_snap_id` is provided, unchanged files
/// (same size + mtime) have their existing span extended rather than creating a
/// new one, achieving span-based deduplication.
///
/// The entire operation is wrapped in a transaction for atomicity and performance.
pub fn index_snapshot(
    db: &Database,
    snap: &DiscoveredSnapshot,
    prev_snap_id: Option<i64>,
) -> Result<IndexResult, Box<dyn std::error::Error>> {
    let scan = scan_directory(&snap.path);

    // Use unchecked_transaction since Database holds a non-mut reference
    let tx = db.conn.unchecked_transaction()?;

    let snap_id =
        db.insert_snapshot(&snap.name, &snap.ts, &snap.source, &snap.path.to_string_lossy())?;

    // Pre-fetch previous snapshot's files for O(1) comparison
    let prev_files: HashMap<String, crate::db::FileRecord> = if let Some(prev_id) = prev_snap_id {
        db.get_files_in_snapshot(prev_id)?
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect()
    } else {
        HashMap::new()
    };

    let mut files_new = 0usize;
    let mut files_extended = 0usize;
    let mut files_changed = 0usize;

    for entry in &scan.entries {
        let file_id =
            db.upsert_file(&entry.path, &entry.name, entry.size, entry.mtime, entry.file_type)?;

        if let Some(prev_file) = prev_files.get(&entry.path) {
            if prev_file.size == entry.size && prev_file.mtime == entry.mtime {
                // File unchanged -- try to extend span
                if let Some(prev_id) = prev_snap_id
                    && db.extend_span(file_id, prev_id, snap_id)?
                {
                    files_extended += 1;
                    continue;
                }
            } else {
                files_changed += 1;
            }
        } else {
            files_new += 1;
        }

        // New file, changed file, or failed extension -- create new span
        db.insert_span(file_id, snap_id, snap_id)?;
    }

    tx.commit()?;

    Ok(IndexResult {
        snapshot_id: snap_id,
        files_total: scan.entries.len(),
        files_new,
        files_extended,
        files_changed,
        scan_errors: scan.errors,
    })
}

/// Walk a backup target, discover all new (unindexed) snapshots, and index them
/// in chronological order so span extension works correctly.
///
/// Pre-populates a HashMap with the latest indexed snapshot per (source, name)
/// pair from the database, then iterates through newly discovered snapshots,
/// passing the predecessor snapshot ID to `index_snapshot()` for span extension.
pub fn walk(
    target_root: &std::path::Path,
    db: &Database,
) -> Result<WalkResult, Box<dyn std::error::Error>> {
    let discovered = discover_snapshots(target_root, db)?;
    let discovered_count = discovered.len();

    let mut results = Vec::new();

    // Track latest indexed snapshot ID per (source, name) for predecessor lookup
    let mut latest_snap_id: HashMap<(String, String), i64> = HashMap::new();

    // Pre-populate from already-indexed snapshots in DB
    for snap in db.list_snapshots()? {
        let key = (snap.source.clone(), snap.name.clone());
        match latest_snap_id.get(&key) {
            Some(&existing_id) => {
                let existing = db.get_snapshot_by_id(existing_id)?;
                if snap.ts > existing.ts {
                    latest_snap_id.insert(key, snap.id);
                }
            }
            None => {
                latest_snap_id.insert(key, snap.id);
            }
        }
    }

    // discovered is already sorted by (source, name, ts) from discover_snapshots
    for snap in &discovered {
        let key = (snap.source.clone(), snap.name.clone());
        let prev_id = latest_snap_id.get(&key).copied();

        let result = index_snapshot(db, snap, prev_id)?;
        latest_snap_id.insert(key, result.snapshot_id);
        results.push(result);
    }

    let total_in_db = db.list_snapshots()?.len();
    let skipped = total_in_db - discovered_count;

    Ok(WalkResult {
        snapshots_discovered: total_in_db,
        snapshots_indexed: discovered_count,
        snapshots_skipped: skipped,
        results,
    })
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

    fn write_file(path: &std::path::Path, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn index_first_snapshot() {
        let tmp = TempDir::new().unwrap();
        let snap_dir = tmp.path().join("nvme/root.20260220T0300");
        write_file(&snap_dir.join("a.txt"), b"aaa");
        write_file(&snap_dir.join("b.txt"), b"bbb");
        write_file(&snap_dir.join("dir/c.txt"), b"ccc");

        let db = Database::open(":memory:").unwrap();
        let snap = DiscoveredSnapshot {
            name: "root".into(),
            ts: "20260220T0300".into(),
            source: "nvme".into(),
            path: snap_dir,
        };
        let result = index_snapshot(&db, &snap, None).unwrap();
        assert_eq!(result.files_new, result.files_total); // all files are new
        assert_eq!(result.files_extended, 0);
        assert!(result.files_total >= 3); // at least 3 files (may include dir entry)
    }

    #[test]
    fn index_extends_spans_for_unchanged_files() {
        let tmp = TempDir::new().unwrap();
        let snap1 = tmp.path().join("nvme/root.20260220T0300");
        let snap2 = tmp.path().join("nvme/root.20260221T0300");
        write_file(&snap1.join("a.txt"), b"same");
        fs::create_dir_all(&snap2).unwrap();
        fs::copy(snap1.join("a.txt"), snap2.join("a.txt")).unwrap();
        // Preserve mtime by using filetime crate
        let meta = fs::metadata(snap1.join("a.txt")).unwrap();
        filetime::set_file_mtime(
            snap2.join("a.txt"),
            filetime::FileTime::from_last_modification_time(&meta),
        )
        .unwrap();

        let db = Database::open(":memory:").unwrap();
        let ds1 = DiscoveredSnapshot {
            name: "root".into(),
            ts: "20260220T0300".into(),
            source: "nvme".into(),
            path: snap1,
        };
        let r1 = index_snapshot(&db, &ds1, None).unwrap();
        let ds2 = DiscoveredSnapshot {
            name: "root".into(),
            ts: "20260221T0300".into(),
            source: "nvme".into(),
            path: snap2,
        };
        let r2 = index_snapshot(&db, &ds2, Some(r1.snapshot_id)).unwrap();
        assert!(r2.files_extended > 0);
        assert_eq!(r2.files_new, 0);
    }

    #[test]
    fn index_detects_new_files() {
        let tmp = TempDir::new().unwrap();
        let snap1 = tmp.path().join("nvme/root.20260220T0300");
        let snap2 = tmp.path().join("nvme/root.20260221T0300");
        write_file(&snap1.join("a.txt"), b"aaa");
        write_file(&snap2.join("a.txt"), b"aaa");
        write_file(&snap2.join("b.txt"), b"new");
        // Preserve mtime on a.txt
        let meta = fs::metadata(snap1.join("a.txt")).unwrap();
        filetime::set_file_mtime(
            snap2.join("a.txt"),
            filetime::FileTime::from_last_modification_time(&meta),
        )
        .unwrap();

        let db = Database::open(":memory:").unwrap();
        let ds1 = DiscoveredSnapshot {
            name: "root".into(),
            ts: "20260220T0300".into(),
            source: "nvme".into(),
            path: snap1,
        };
        let r1 = index_snapshot(&db, &ds1, None).unwrap();
        let ds2 = DiscoveredSnapshot {
            name: "root".into(),
            ts: "20260221T0300".into(),
            source: "nvme".into(),
            path: snap2,
        };
        let r2 = index_snapshot(&db, &ds2, Some(r1.snapshot_id)).unwrap();
        assert!(r2.files_new >= 1); // b.txt is new
        assert!(r2.files_extended >= 1); // a.txt extended
    }

    #[test]
    fn index_detects_changed_files() {
        let tmp = TempDir::new().unwrap();
        let snap1 = tmp.path().join("nvme/root.20260220T0300");
        let snap2 = tmp.path().join("nvme/root.20260221T0300");
        write_file(&snap1.join("a.txt"), b"old content");
        write_file(&snap2.join("a.txt"), b"new longer content here");

        let db = Database::open(":memory:").unwrap();
        let ds1 = DiscoveredSnapshot {
            name: "root".into(),
            ts: "20260220T0300".into(),
            source: "nvme".into(),
            path: snap1,
        };
        let r1 = index_snapshot(&db, &ds1, None).unwrap();
        let ds2 = DiscoveredSnapshot {
            name: "root".into(),
            ts: "20260221T0300".into(),
            source: "nvme".into(),
            path: snap2,
        };
        let r2 = index_snapshot(&db, &ds2, Some(r1.snapshot_id)).unwrap();
        assert!(r2.files_changed >= 1);
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

    #[test]
    fn walk_indexes_all_new_snapshots() {
        let tmp = TempDir::new().unwrap();
        make_snap_dirs(&tmp, &["nvme/root.20260220T0300", "ssd/opt.20260220T0300"]);
        write_file(&tmp.path().join("nvme/root.20260220T0300/a.txt"), b"a");
        write_file(&tmp.path().join("ssd/opt.20260220T0300/b.txt"), b"b");

        let db = Database::open(":memory:").unwrap();
        let result = walk(tmp.path(), &db).unwrap();
        assert_eq!(result.snapshots_indexed, 2);
        assert_eq!(result.snapshots_skipped, 0);
    }

    #[test]
    fn walk_skips_already_indexed() {
        let tmp = TempDir::new().unwrap();
        make_snap_dirs(&tmp, &["nvme/root.20260220T0300", "nvme/root.20260221T0300"]);
        write_file(&tmp.path().join("nvme/root.20260220T0300/a.txt"), b"a");
        write_file(&tmp.path().join("nvme/root.20260221T0300/a.txt"), b"a");

        let db = Database::open(":memory:").unwrap();
        let r1 = walk(tmp.path(), &db).unwrap();
        assert_eq!(r1.snapshots_indexed, 2);
        let r2 = walk(tmp.path(), &db).unwrap();
        assert_eq!(r2.snapshots_indexed, 0);
    }

    #[test]
    fn walk_orders_by_timestamp() {
        let tmp = TempDir::new().unwrap();
        // Create in reverse order to verify sorting
        make_snap_dirs(
            &tmp,
            &[
                "nvme/root.20260222T0300",
                "nvme/root.20260220T0300",
                "nvme/root.20260221T0300",
            ],
        );
        for d in &["20260220T0300", "20260221T0300", "20260222T0300"] {
            write_file(
                &tmp.path().join(format!("nvme/root.{}/a.txt", d)),
                b"a",
            );
        }

        let db = Database::open(":memory:").unwrap();
        let result = walk(tmp.path(), &db).unwrap();
        assert_eq!(result.snapshots_indexed, 3);
        let snaps = db.list_snapshots().unwrap();
        assert!(snaps[0].ts < snaps[1].ts);
        assert!(snaps[1].ts < snaps[2].ts);
    }
}
