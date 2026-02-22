//! End-to-end integration tests for the ButteredDASD content indexer.
//!
//! Each test exercises the full pipeline — directory creation, walk/index,
//! database queries — using in-memory SQLite databases and `tempfile::TempDir`
//! for filesystem fixtures.

use buttered_dasd::db::Database;
use buttered_dasd::indexer;
use buttered_dasd::indexer::{DiscoveredSnapshot, index_snapshot};
use std::fs;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write `content` bytes to `path`, creating parent directories as needed.
fn write_file(path: &std::path::Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Copy a file and preserve its mtime so span extension triggers correctly.
fn copy_with_mtime(src: &std::path::Path, dst: &std::path::Path) {
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    fs::copy(src, dst).unwrap();
    let meta = fs::metadata(src).unwrap();
    filetime::set_file_mtime(dst, filetime::FileTime::from_last_modification_time(&meta)).unwrap();
}

// ---------------------------------------------------------------------------
// test_full_walk_pipeline
// ---------------------------------------------------------------------------

/// Create a backup target with two source directories, each containing multiple
/// snapshot directories, populate them with files, then run `walk()` and verify:
///
/// - All snapshots are discovered and indexed (none skipped).
/// - The database contains one row per snapshot.
/// - Every file written is recorded in `files`.
/// - At least one span row exists per file.
#[test]
fn test_full_walk_pipeline() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // nvme source: two snapshots
    write_file(
        &root.join("nvme/root.20260220T0300/etc/passwd"),
        b"root:x:0:0",
    );
    write_file(
        &root.join("nvme/root.20260220T0300/home/alice/.bashrc"),
        b"# bashrc",
    );
    write_file(&root.join("nvme/root.20260220T0300/usr/bin/sh"), b"\x7fELF");

    write_file(
        &root.join("nvme/root.20260221T0300/etc/passwd"),
        b"root:x:0:0",
    );
    write_file(
        &root.join("nvme/root.20260221T0300/home/alice/.bashrc"),
        b"# bashrc",
    );
    write_file(
        &root.join("nvme/root.20260221T0300/home/alice/notes.txt"),
        b"new note",
    );

    // ssd source: one snapshot
    write_file(
        &root.join("ssd/opt.20260220T0300/app/config.yaml"),
        b"key: val",
    );
    write_file(
        &root.join("ssd/opt.20260220T0300/app/data.bin"),
        b"\x00\x01\x02",
    );

    let db = Database::open(":memory:").unwrap();
    let result = indexer::walk(root, &db).unwrap();

    // 3 snapshot directories exist; all should be freshly indexed
    assert_eq!(
        result.snapshots_indexed, 3,
        "expected 3 newly indexed snapshots"
    );
    assert_eq!(
        result.snapshots_skipped, 0,
        "no snapshots should be skipped"
    );
    assert_eq!(
        result.snapshots_discovered, 3,
        "total discovered (db count) must be 3"
    );
    assert_eq!(result.results.len(), 3, "one IndexResult per snapshot");

    // All IndexResults should have recorded files
    for r in &result.results {
        assert!(
            r.files_total >= 1,
            "each snapshot must have at least 1 file, got {}",
            r.files_total
        );
    }

    // Database consistency: snapshots table
    let snaps = db.list_snapshots().unwrap();
    assert_eq!(snaps.len(), 3);

    // Every file must appear in the files table
    let file_count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert!(
        file_count >= 5,
        "expected at least 5 distinct files, got {}",
        file_count
    );

    // Every file must have at least one span
    let orphan_files: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE id NOT IN (SELECT file_id FROM spans)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphan_files, 0, "every file must have a span");
}

// ---------------------------------------------------------------------------
// test_incremental_indexing
// ---------------------------------------------------------------------------

/// Walk once, add a new snapshot directory with new files, walk again.
/// Verify that:
///
/// - The second walk indexes exactly 1 new snapshot.
/// - Files from the first walk are not re-indexed (skipped via snapshot_exists).
/// - Only files from the new snapshot appear as `files_new` in the second result.
#[test]
fn test_incremental_indexing() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Initial state: one snapshot
    write_file(&root.join("nvme/root.20260220T0300/a.txt"), b"alpha");
    write_file(&root.join("nvme/root.20260220T0300/b.txt"), b"beta");

    let db = Database::open(":memory:").unwrap();
    let r1 = indexer::walk(root, &db).unwrap();
    assert_eq!(r1.snapshots_indexed, 1, "first walk: 1 snapshot");
    assert_eq!(r1.snapshots_skipped, 0);

    // Add a second snapshot with one unchanged file and one new file
    let snap1_a = root.join("nvme/root.20260220T0300/a.txt");
    let snap2_a = root.join("nvme/root.20260221T0300/a.txt");
    copy_with_mtime(&snap1_a, &snap2_a);
    write_file(
        &root.join("nvme/root.20260221T0300/c.txt"),
        b"gamma - new file",
    );

    let r2 = indexer::walk(root, &db).unwrap();
    assert_eq!(r2.snapshots_indexed, 1, "second walk: only 1 new snapshot");
    assert_eq!(
        r2.snapshots_skipped, 1,
        "second walk: 1 already-indexed snapshot skipped"
    );

    // The single IndexResult for the new snapshot
    let ir = &r2.results[0];

    // c.txt is genuinely new; a.txt should be extended (not new)
    assert!(ir.files_new >= 1, "c.txt must be counted as new");
    assert!(
        ir.files_extended >= 1,
        "a.txt (unchanged) must be extended, got extended={}",
        ir.files_extended
    );
    assert_eq!(ir.files_changed, 0, "no files changed between snapshots");

    // Total snapshot count in DB: 2
    assert_eq!(db.list_snapshots().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// test_search_after_indexing
// ---------------------------------------------------------------------------

/// Walk a target with a variety of file names, then use `database.search()` to
/// find files by:
///
/// - Exact filename term.
/// - Prefix wildcard (`*`).
/// - Path component (directory name).
#[test]
fn test_search_after_indexing() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    write_file(
        &root.join("nvme/root.20260220T0300/docs/annual-report.pdf"),
        b"PDF content",
    );
    write_file(
        &root.join("nvme/root.20260220T0300/docs/budget-2026.xlsx"),
        b"XLS content",
    );
    write_file(
        &root.join("nvme/root.20260220T0300/photos/vacation.jpg"),
        b"JPEG content",
    );
    write_file(
        &root.join("nvme/root.20260220T0300/photos/family.jpg"),
        b"JPEG content 2",
    );
    write_file(
        &root.join("nvme/root.20260220T0300/src/main.rs"),
        b"fn main() {}",
    );

    let db = Database::open(":memory:").unwrap();
    indexer::walk(root, &db).unwrap();

    // Search by filename term — "report" should match annual-report.pdf
    let results = db.search("report", 50).unwrap();
    assert!(
        !results.is_empty(),
        "search for 'report' must return at least one result"
    );
    let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("report")),
        "results must include annual-report.pdf, got: {:?}",
        names
    );

    // Search by prefix wildcard — "budget*" should match budget-2026.xlsx
    let budget_results = db.search("budget*", 50).unwrap();
    assert!(
        !budget_results.is_empty(),
        "prefix search 'budget*' must return results"
    );

    // Search by path component — "photos" should find both JPEGs
    let photo_results = db.search("photos", 50).unwrap();
    assert!(
        photo_results.len() >= 2,
        "search for 'photos' (directory) must return >=2 results, got {}",
        photo_results.len()
    );

    // Verify that search results include snapshot span information
    for r in &results {
        assert!(
            !r.first_snap.is_empty(),
            "first_snap must be populated in search results"
        );
        assert!(
            !r.last_snap.is_empty(),
            "last_snap must be populated in search results"
        );
    }

    // Search for something that doesn't exist
    let empty = db.search("nonexistent_xyzzy", 50).unwrap();
    assert!(
        empty.is_empty(),
        "search for gibberish must return no results"
    );
}

// ---------------------------------------------------------------------------
// test_changed_file_detection
// ---------------------------------------------------------------------------

/// Create snapshot A with `file.txt` at 100 bytes, snapshot B with `file.txt`
/// at 200 bytes. Walk both and verify:
///
/// - The file has TWO spans (one per snapshot), not a single extended span.
/// - `files_changed` is 1 in the second snapshot's IndexResult.
/// - The `files` table records the file only once (same path — upserted).
#[test]
fn test_changed_file_detection() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Snapshot A: file.txt is 100 bytes
    let snap_a = root.join("nvme/root.20260220T0300");
    write_file(&snap_a.join("file.txt"), &[b'A'; 100]);

    // Snapshot B: file.txt is 200 bytes (different size forces a new span)
    let snap_b = root.join("nvme/root.20260221T0300");
    write_file(&snap_b.join("file.txt"), &[b'B'; 200]);

    let db = Database::open(":memory:").unwrap();
    let result = indexer::walk(root, &db).unwrap();

    assert_eq!(result.snapshots_indexed, 2);

    // Second snapshot's result must report the file as changed
    // (walk processes in timestamp order, so results[0]=snap_a, results[1]=snap_b)
    let ir_b = &result.results[1];
    assert!(
        ir_b.files_changed >= 1,
        "file.txt size change must be detected, files_changed={}",
        ir_b.files_changed
    );
    assert_eq!(
        ir_b.files_extended, 0,
        "file.txt must NOT be extended when size differs"
    );

    // The files table has exactly one row for file.txt (upserted, not duplicated)
    let file_count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE name = 'file.txt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(file_count, 1, "file.txt must have exactly one files row");

    // But spans: two rows — one per snapshot (no extension happened)
    let file_id: i64 = db
        .conn
        .query_row("SELECT id FROM files WHERE name = 'file.txt'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let span_count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM spans WHERE file_id = ?1",
            [file_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        span_count, 2,
        "changed file must have 2 separate spans, got {}",
        span_count
    );
}

// ---------------------------------------------------------------------------
// test_cli_info_output  (via Database::get_stats)
// ---------------------------------------------------------------------------

/// Index a known set of snapshots and files, then call `Database::get_stats()`
/// and verify the counts match expectations without shelling out to the binary.
#[test]
fn test_cli_info_output() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Two source dirs, each with one snapshot and a distinct set of files
    // nvme: 4 files
    write_file(&root.join("nvme/root.20260220T0300/bin/sh"), b"\x7fELF");
    write_file(&root.join("nvme/root.20260220T0300/etc/fstab"), b"# fstab");
    write_file(
        &root.join("nvme/root.20260220T0300/etc/hostname"),
        b"myhost",
    );
    write_file(
        &root.join("nvme/root.20260220T0300/home/user/.profile"),
        b"export PATH",
    );

    // ssd: 2 files
    write_file(&root.join("ssd/opt.20260220T0300/app.jar"), b"PK\x03\x04");
    write_file(&root.join("ssd/opt.20260220T0300/config.toml"), b"[app]");

    let db = Database::open(":memory:").unwrap();
    indexer::walk(root, &db).unwrap();

    let stats = db.get_stats().unwrap();

    // snapshot_count must equal 2 (one per source/snapshot directory)
    assert_eq!(
        stats.snapshot_count, 2,
        "expected 2 snapshots, got {}",
        stats.snapshot_count
    );

    // file_count: scanner also records directory entries (file_type=1),
    // so we get at least the 6 regular files we wrote.
    assert!(
        stats.file_count >= 6,
        "expected at least 6 files indexed, got {}",
        stats.file_count
    );

    // span_count: every file must have exactly one span (first walk, no predecessors)
    assert_eq!(
        stats.span_count, stats.file_count,
        "on first walk every file gets exactly one span"
    );

    // db_size must be > 0 (database has been written to)
    assert!(stats.db_size > 0, "db_size must be positive");
}

// ---------------------------------------------------------------------------
// test_multi_source_span_independence
// ---------------------------------------------------------------------------

/// Verify that span extension does NOT bleed across source directories.
/// A file named `config.toml` in `nvme/root` and `ssd/opt` must each have
/// their own independent span; they are different paths so they share a `files`
/// row only if their relative path AND name are identical, but the snapshot
/// context is different, so spans must be independent.
#[test]
fn test_multi_source_span_independence() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Same relative path in two different source snapshots
    write_file(&root.join("nvme/root.20260220T0300/config.toml"), b"[nvme]");
    write_file(&root.join("ssd/opt.20260220T0300/config.toml"), b"[ssd]");

    let db = Database::open(":memory:").unwrap();
    indexer::walk(root, &db).unwrap();

    // Both files have path "config.toml" (relative within their snapshot).
    // Because the scanner stores only the relative path, these collide in the
    // files table — same relative path, same name.  That is expected behaviour:
    // the files table is a content-addressed deduplicated store keyed on path.
    // What matters is that two distinct spans exist (one per snapshot), each
    // anchored to its own snapshot ID.
    let span_count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM spans", [], |r| r.get(0))
        .unwrap();
    assert!(
        span_count >= 2,
        "at least 2 spans expected for 2 snapshots, got {}",
        span_count
    );

    // Snapshot count must be exactly 2
    assert_eq!(db.list_snapshots().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// test_incremental_extends_existing_spans
// ---------------------------------------------------------------------------

/// Walk three consecutive snapshots where the same file is UNCHANGED across
/// all three.  Verify that the file ends up with exactly ONE span whose
/// first_snap is the earliest and last_snap is the latest snapshot.
#[test]
fn test_incremental_extends_existing_spans() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Snapshot 1
    let snap1 = root.join("nvme/root.20260220T0300");
    write_file(&snap1.join("stable.txt"), b"I never change");

    let db = Database::open(":memory:").unwrap();

    // Walk snapshot 1
    let r1 = indexer::walk(root, &db).unwrap();
    assert_eq!(r1.snapshots_indexed, 1);

    // Snapshot 2: copy stable.txt preserving mtime
    let snap2 = root.join("nvme/root.20260221T0300");
    copy_with_mtime(&snap1.join("stable.txt"), &snap2.join("stable.txt"));
    let r2 = indexer::walk(root, &db).unwrap();
    assert_eq!(r2.snapshots_indexed, 1);
    assert!(
        r2.results[0].files_extended >= 1,
        "span must extend at snap2"
    );

    // Snapshot 3: copy stable.txt preserving mtime again
    let snap3 = root.join("nvme/root.20260222T0300");
    copy_with_mtime(&snap2.join("stable.txt"), &snap3.join("stable.txt"));
    let r3 = indexer::walk(root, &db).unwrap();
    assert_eq!(r3.snapshots_indexed, 1);
    assert!(
        r3.results[0].files_extended >= 1,
        "span must extend at snap3"
    );

    // Verify the file has exactly ONE span in the database
    let file_id: i64 = db
        .conn
        .query_row("SELECT id FROM files WHERE name = 'stable.txt'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let span_count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM spans WHERE file_id = ?1",
            [file_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        span_count, 1,
        "unchanged file across 3 snapshots must have exactly 1 span"
    );

    // The single span must cover first_snap=snap1 and last_snap=snap3
    let snaps = db.list_snapshots().unwrap();
    let snap1_id = snaps[0].id;
    let snap3_id = snaps[2].id;
    let (first, last): (i64, i64) = db
        .conn
        .query_row(
            "SELECT first_snap, last_snap FROM spans WHERE file_id = ?1",
            [file_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(first, snap1_id, "span.first_snap must be snapshot 1");
    assert_eq!(last, snap3_id, "span.last_snap must be snapshot 3");
}

// ---------------------------------------------------------------------------
// test_walk_result_counts_consistency
// ---------------------------------------------------------------------------

/// Verify internal consistency of WalkResult: the sum of files across all
/// IndexResults equals the total file count tracked by `get_stats()` when
/// all files are unique across snapshots (no deduplication).
#[test]
fn test_walk_result_counts_consistency() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Two snapshots, entirely unique files in each
    write_file(&root.join("nvme/root.20260220T0300/alpha.txt"), b"alpha");
    write_file(&root.join("nvme/root.20260220T0300/beta.txt"), b"beta");
    write_file(&root.join("nvme/root.20260221T0300/gamma.txt"), b"gamma");
    write_file(&root.join("nvme/root.20260221T0300/delta.txt"), b"delta");

    let db = Database::open(":memory:").unwrap();
    let result = indexer::walk(root, &db).unwrap();

    assert_eq!(result.snapshots_indexed, 2);

    // Sum of files_total across all IndexResults
    let total_from_results: usize = result.results.iter().map(|r| r.files_total).sum();

    // The stats file_count reflects deduplicated files in the `files` table.
    // Since all files are unique, they must match.
    let stats = db.get_stats().unwrap();
    assert_eq!(
        total_from_results as i64, stats.file_count,
        "sum of files_total in walk results must equal DB file count when all files unique"
    );

    // All files in snap1 are new, all in snap2 are new (no shared files)
    let total_new: usize = result.results.iter().map(|r| r.files_new).sum();
    let total_extended: usize = result.results.iter().map(|r| r.files_extended).sum();
    assert_eq!(
        total_new, total_from_results,
        "all files must be new when no overlap between snapshots"
    );
    assert_eq!(total_extended, 0, "no files should be extended");
}

// ---------------------------------------------------------------------------
// test_index_snapshot_directly
// ---------------------------------------------------------------------------

/// Exercise `index_snapshot()` directly (bypassing `walk()`) to test the
/// predecessor-pass mechanism for span extension in isolation.
#[test]
fn test_index_snapshot_directly() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Build two snapshot dirs manually
    let snap_a_path = root.join("nvme/snap_a.20260220T0300");
    let snap_b_path = root.join("nvme/snap_b.20260221T0300");

    write_file(&snap_a_path.join("readme.txt"), b"version 1");
    write_file(&snap_a_path.join("data.bin"), b"\x00\x01\x02\x03");

    // snap_b: readme.txt unchanged, data.bin modified
    copy_with_mtime(
        &snap_a_path.join("readme.txt"),
        &snap_b_path.join("readme.txt"),
    );
    write_file(&snap_b_path.join("data.bin"), b"\xFF\xFE\xFD\xFC\xFB");

    let db = Database::open(":memory:").unwrap();

    let ds_a = DiscoveredSnapshot {
        name: "snap_a".into(),
        ts: "20260220T0300".into(),
        source: "nvme".into(),
        path: snap_a_path,
    };
    let ra = index_snapshot(&db, &ds_a, None).unwrap();

    assert_eq!(
        ra.files_new, ra.files_total,
        "all files new in first snapshot"
    );
    assert_eq!(ra.files_extended, 0);
    assert_eq!(ra.files_changed, 0);

    let ds_b = DiscoveredSnapshot {
        name: "snap_b".into(),
        ts: "20260221T0300".into(),
        source: "nvme".into(),
        path: snap_b_path,
    };
    let rb = index_snapshot(&db, &ds_b, Some(ra.snapshot_id)).unwrap();

    assert!(rb.files_extended >= 1, "readme.txt must be extended");
    assert!(
        rb.files_changed >= 1,
        "data.bin must be detected as changed"
    );
    assert_eq!(rb.files_new, 0, "no entirely new files in snap_b");

    // Snapshot IDs must be distinct positive integers
    assert!(ra.snapshot_id > 0);
    assert!(rb.snapshot_id > 0);
    assert_ne!(ra.snapshot_id, rb.snapshot_id);
}
