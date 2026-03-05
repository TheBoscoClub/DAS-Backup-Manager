# Content Indexer Implementation Plan (Rust)

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a Rust CLI tool (`das-index`) that indexes file metadata from btrbk snapshots into a SQLite FTS5 database, using span-based storage to efficiently track files across hundreds of snapshots.

**Architecture:** Three-layer design — Database (rusqlite with FTS5 and WAL), Scanner (walkdir-based filesystem walker), and Indexer (orchestrator that discovers new snapshots on a backup target, walks them, and updates the DB with span logic). The CLI uses clap to dispatch subcommands: `walk`, `search`, `list`, `info`. The Rust binary lives in `indexer/` as a standalone Cargo project, separate from the top-level CMake build (which handles scripts/systemd/future C++ GUI).

**Tech Stack:** Rust 1.93, rusqlite 0.38 (system SQLite 3.51 with FTS5), clap 4.5, walkdir 2.5, tempfile (dev), cargo test

---

## Snapshot Structure Reference

btrbk snapshots live on the backup target at paths like:
```
/mnt/backup-22tb/
  nvme/
    root.20260221T0304/       (snapshot directory, full filesystem tree)
    home.20260221T0304/
    root-home.20260221T0304/
    log.20260221T0304/
  ssd/
    opt.20260221T0304/
    srv.20260221T0304/
  projects/
    claude-projects.20260221T0304/
  audiobooks/
    audiobooks-sources.20260221T0304/
  das-storage/
    das-data.20260221T0304/
```

Snapshot name format: `<name>.<YYYYMMDDTHHMMSS>`
The name portion matches btrbk's `snapshot_name` config (e.g., `root`, `home`, `opt`).
Each snapshot directory contains the complete filesystem tree of that subvolume at that timestamp.

## Database Schema

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS snapshots (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL,          -- "root", "home", "opt", etc.
    ts      TEXT NOT NULL,          -- "20260221T0304" (btrbk timestamp)
    source  TEXT NOT NULL,          -- "nvme", "ssd", "projects", etc.
    path    TEXT NOT NULL UNIQUE,   -- full snapshot dir path on target
    indexed_at INTEGER NOT NULL     -- epoch when we indexed this snapshot
);

CREATE TABLE IF NOT EXISTS files (
    id    INTEGER PRIMARY KEY,
    path  TEXT NOT NULL,            -- path within snapshot (e.g. "home/bosco/.zshrc")
    name  TEXT NOT NULL,            -- basename (e.g. ".zshrc")
    size  INTEGER NOT NULL DEFAULT 0,
    mtime INTEGER NOT NULL DEFAULT 0,
    type  INTEGER NOT NULL DEFAULT 0  -- 0=regular, 1=directory, 2=symlink, 3=other
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_files_path ON files(path);

CREATE TABLE IF NOT EXISTS spans (
    file_id    INTEGER NOT NULL REFERENCES files(id),
    first_snap INTEGER NOT NULL REFERENCES snapshots(id),
    last_snap  INTEGER NOT NULL REFERENCES snapshots(id),
    PRIMARY KEY (file_id, first_snap)
);

CREATE INDEX IF NOT EXISTS idx_spans_last ON spans(last_snap);

CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
    name, path, content=files, content_rowid=id
);

-- Triggers to keep FTS5 in sync with files table
CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
    INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, name, path) VALUES('delete', old.id, old.name, old.path);
END;
CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, name, path) VALUES('delete', old.id, old.name, old.path);
    INSERT INTO files_fts(files_fts, rowid, name, path) VALUES (new.id, new.name, new.path);
END;
```

## Span Logic

**Key invariant:** For a given file path, if it exists unchanged (same size+mtime) in consecutive snapshots S1, S2, S3, there is exactly ONE span row: `{file_id, first_snap=S1, last_snap=S3}`.

When indexing snapshot Sn:
1. Walk the snapshot directory tree, collecting `{path, name, size, mtime, type}` for every entry
2. For each entry, look up the file by path in `files` table:
   - **New file (not in table):** INSERT into `files`, INSERT new span `{file_id, first_snap=Sn, last_snap=Sn}`
   - **Existing file, unchanged (same size+mtime):** Find the span where `last_snap = Sn-1`, UPDATE `last_snap = Sn`
   - **Existing file, changed (different size or mtime):** UPDATE `files` row with new size/mtime, INSERT new span `{file_id, first_snap=Sn, last_snap=Sn}`
3. Files that existed in Sn-1 but NOT in Sn: their spans naturally end at Sn-1 (no action needed, absence = deletion)

---

## Task 1: Cargo project scaffold

**Files:**
- Create: `indexer/Cargo.toml`
- Create: `indexer/src/main.rs`
- Create: `indexer/src/lib.rs`
- Create: `indexer/src/db.rs`
- Create: `indexer/src/scanner.rs`
- Create: `indexer/src/indexer.rs`

**Step 1: Create `indexer/Cargo.toml`**

```toml
[package]
name = "das-index"
version = "0.1.0"
edition = "2024"
description = "Content indexer for DAS backup snapshots — SQLite FTS5 with span-based storage"
license = "GPL-3.0"

[dependencies]
rusqlite = { version = "0.38", features = ["bundled"] }
clap = { version = "4.5", features = ["derive"] }
walkdir = "2.5"
regex = "1"

[dev-dependencies]
tempfile = "3"
```

Note: We use `bundled` feature for rusqlite to compile SQLite from source with all extensions (FTS5) guaranteed, avoiding any system library version mismatch issues.

**Step 2: Create `indexer/src/main.rs`**

```rust
fn main() {
    println!("das-index: content indexer for DAS backup snapshots");
}
```

**Step 3: Create `indexer/src/lib.rs`**

```rust
pub mod db;
pub mod indexer;
pub mod scanner;
```

**Step 4: Create stub modules**

`indexer/src/db.rs`:
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn stub_compiles() {
        assert!(true);
    }
}
```

`indexer/src/scanner.rs` and `indexer/src/indexer.rs`: same stub.

**Step 5: Build and test**

Run: `cargo build --manifest-path indexer/Cargo.toml`
Run: `cargo test --manifest-path indexer/Cargo.toml`
Expected: Build succeeds, 3 stub tests pass.

**Step 6: Commit**

```bash
git add indexer/
git commit -m "feat(indexer): scaffold Rust Cargo project with dependencies"
```

---

## Task 2: Database layer — schema and connection

**Files:**
- Modify: `indexer/src/db.rs`

**Step 1: Write failing tests for DB initialization**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_in_memory() {
        let db = Database::open(":memory:").unwrap();
        assert!(db.conn.is_autocommit());
    }

    #[test]
    fn creates_schema() {
        let db = Database::open(":memory:").unwrap();
        let tables: Vec<String> = db.conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"snapshots".to_string()));
        assert!(tables.contains(&"files".to_string()));
        assert!(tables.contains(&"spans".to_string()));
        assert!(tables.contains(&"files_fts".to_string()));
    }

    #[test]
    fn wal_mode() {
        let db = Database::open(":memory:").unwrap();
        let mode: String = db.conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn foreign_keys_enabled() {
        let db = Database::open(":memory:").unwrap();
        let fk: i64 = db.conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }
}
```

**Step 2: Run tests, verify they fail**

Run: `cargo test --manifest-path indexer/Cargo.toml -- db::tests`
Expected: FAIL — `Database` struct doesn't exist yet.

**Step 3: Implement Database struct**

```rust
use rusqlite::{Connection, Result as SqlResult};
use std::path::Path;

const SCHEMA_SQL: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS snapshots (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL,
    ts      TEXT NOT NULL,
    source  TEXT NOT NULL,
    path    TEXT NOT NULL UNIQUE,
    indexed_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id    INTEGER PRIMARY KEY,
    path  TEXT NOT NULL,
    name  TEXT NOT NULL,
    size  INTEGER NOT NULL DEFAULT 0,
    mtime INTEGER NOT NULL DEFAULT 0,
    type  INTEGER NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_files_path ON files(path);

CREATE TABLE IF NOT EXISTS spans (
    file_id    INTEGER NOT NULL REFERENCES files(id),
    first_snap INTEGER NOT NULL REFERENCES snapshots(id),
    last_snap  INTEGER NOT NULL REFERENCES snapshots(id),
    PRIMARY KEY (file_id, first_snap)
);

CREATE INDEX IF NOT EXISTS idx_spans_last ON spans(last_snap);

CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
    name, path, content=files, content_rowid=id
);

CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
    INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, name, path) VALUES('delete', old.id, old.name, old.path);
END;
CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, name, path) VALUES('delete', old.id, old.name, old.path);
    INSERT INTO files_fts(files_fts, rowid, name, path) VALUES (new.id, new.name, new.path);
END;
"#;

pub struct Database {
    pub conn: Connection,
}

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> SqlResult<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Database { conn })
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = self.conn.execute_batch("PRAGMA optimize;");
    }
}
```

**Step 4: Run tests, verify pass**

Run: `cargo test --manifest-path indexer/Cargo.toml -- db::tests`
Expected: 4 tests PASS.

**Step 5: Commit**

```bash
git add indexer/src/db.rs
git commit -m "feat(indexer): database layer with schema init, WAL, FTS5"
```

---

## Task 3: Database layer — snapshot CRUD

**Files:**
- Modify: `indexer/src/db.rs`

**Step 1: Write failing tests**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub id: i64,
    pub name: String,
    pub ts: String,
    pub source: String,
    pub path: String,
    pub indexed_at: i64,
}

#[cfg(test)]
mod tests {
    // ... existing tests ...

    #[test]
    fn insert_snapshot() {
        let db = Database::open(":memory:").unwrap();
        let id = db.insert_snapshot("root", "20260221T0304", "nvme", "/mnt/backup/nvme/root.20260221T0304").unwrap();
        assert!(id > 0);
    }

    #[test]
    fn get_snapshot_by_path() {
        let db = Database::open(":memory:").unwrap();
        db.insert_snapshot("root", "20260221T0304", "nvme", "/mnt/backup/nvme/root.20260221T0304").unwrap();
        let snap = db.get_snapshot("/mnt/backup/nvme/root.20260221T0304").unwrap().unwrap();
        assert_eq!(snap.name, "root");
        assert_eq!(snap.ts, "20260221T0304");
        assert_eq!(snap.source, "nvme");
    }

    #[test]
    fn snapshot_exists() {
        let db = Database::open(":memory:").unwrap();
        db.insert_snapshot("root", "20260221T0304", "nvme", "/mnt/backup/nvme/root.20260221T0304").unwrap();
        assert!(db.snapshot_exists("/mnt/backup/nvme/root.20260221T0304").unwrap());
        assert!(!db.snapshot_exists("/mnt/backup/nvme/nonexistent").unwrap());
    }

    #[test]
    fn list_snapshots() {
        let db = Database::open(":memory:").unwrap();
        db.insert_snapshot("root", "20260220T0300", "nvme", "/mnt/backup/nvme/root.20260220T0300").unwrap();
        db.insert_snapshot("root", "20260221T0300", "nvme", "/mnt/backup/nvme/root.20260221T0300").unwrap();
        db.insert_snapshot("home", "20260221T0300", "nvme", "/mnt/backup/nvme/home.20260221T0300").unwrap();
        let snaps = db.list_snapshots().unwrap();
        assert_eq!(snaps.len(), 3);
        assert_eq!(snaps[0].ts, "20260220T0300"); // ordered by ts
    }
}
```

**Step 2: Run, verify fail**

Run: `cargo test --manifest-path indexer/Cargo.toml -- db::tests`
Expected: FAIL — methods don't exist.

**Step 3: Implement**

Add to `Database` impl:

```rust
pub fn insert_snapshot(&self, name: &str, ts: &str, source: &str, path: &str) -> SqlResult<i64> {
    let indexed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    self.conn.execute(
        "INSERT INTO snapshots (name, ts, source, path, indexed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![name, ts, source, path, indexed_at],
    )?;
    Ok(self.conn.last_insert_rowid())
}

pub fn snapshot_exists(&self, path: &str) -> SqlResult<bool> {
    let count: i64 = self.conn.query_row(
        "SELECT COUNT(*) FROM snapshots WHERE path = ?1",
        [path],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub fn get_snapshot(&self, path: &str) -> SqlResult<Option<Snapshot>> {
    let mut stmt = self.conn.prepare(
        "SELECT id, name, ts, source, path, indexed_at FROM snapshots WHERE path = ?1"
    )?;
    let mut rows = stmt.query_map([path], |row| {
        Ok(Snapshot {
            id: row.get(0)?,
            name: row.get(1)?,
            ts: row.get(2)?,
            source: row.get(3)?,
            path: row.get(4)?,
            indexed_at: row.get(5)?,
        })
    })?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn list_snapshots(&self) -> SqlResult<Vec<Snapshot>> {
    let mut stmt = self.conn.prepare(
        "SELECT id, name, ts, source, path, indexed_at FROM snapshots ORDER BY ts"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Snapshot {
            id: row.get(0)?,
            name: row.get(1)?,
            ts: row.get(2)?,
            source: row.get(3)?,
            path: row.get(4)?,
            indexed_at: row.get(5)?,
        })
    })?;
    rows.collect()
}
```

**Step 4: Run, verify pass**

Run: `cargo test --manifest-path indexer/Cargo.toml -- db::tests`
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add indexer/src/db.rs
git commit -m "feat(indexer): snapshot CRUD operations"
```

---

## Task 4: Database layer — file CRUD and span operations

**Files:**
- Modify: `indexer/src/db.rs`

**Step 1: Write failing tests**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub size: i64,
    pub mtime: i64,
    pub file_type: i32,
}

#[cfg(test)]
mod tests {
    // ... existing tests ...

    #[test]
    fn upsert_file_new() {
        let db = Database::open(":memory:").unwrap();
        let id = db.upsert_file("home/bosco/.zshrc", ".zshrc", 1024, 1708500000, 0).unwrap();
        assert!(id > 0);
    }

    #[test]
    fn get_file_by_path() {
        let db = Database::open(":memory:").unwrap();
        db.upsert_file("home/bosco/.zshrc", ".zshrc", 1024, 1708500000, 0).unwrap();
        let f = db.get_file("home/bosco/.zshrc").unwrap().unwrap();
        assert_eq!(f.name, ".zshrc");
        assert_eq!(f.size, 1024);
    }

    #[test]
    fn upsert_file_unchanged() {
        let db = Database::open(":memory:").unwrap();
        let id1 = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        let id2 = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        assert_eq!(id1, id2); // same ID, no duplicate
    }

    #[test]
    fn upsert_file_changed() {
        let db = Database::open(":memory:").unwrap();
        let id1 = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        let id2 = db.upsert_file("a.txt", "a.txt", 200, 2000, 0).unwrap();
        assert_eq!(id1, id2); // same ID
        let f = db.get_file("a.txt").unwrap().unwrap();
        assert_eq!(f.size, 200); // updated
        assert_eq!(f.mtime, 2000);
    }

    #[test]
    fn insert_span() {
        let db = Database::open(":memory:").unwrap();
        let snap_id = db.insert_snapshot("root", "20260221T0304", "nvme", "/snap1").unwrap();
        let file_id = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        db.insert_span(file_id, snap_id, snap_id).unwrap();
        // Verify span exists
        let count: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM spans WHERE file_id = ?1", [file_id], |r| r.get(0)
        ).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn extend_span() {
        let db = Database::open(":memory:").unwrap();
        let s1 = db.insert_snapshot("root", "20260220T0300", "nvme", "/snap1").unwrap();
        let s2 = db.insert_snapshot("root", "20260221T0300", "nvme", "/snap2").unwrap();
        let fid = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        db.insert_span(fid, s1, s1).unwrap();
        let extended = db.extend_span(fid, s1, s2).unwrap();
        assert!(extended);
        let last: i64 = db.conn.query_row(
            "SELECT last_snap FROM spans WHERE file_id = ?1 AND first_snap = ?2",
            rusqlite::params![fid, s1], |r| r.get(0)
        ).unwrap();
        assert_eq!(last, s2);
    }

    #[test]
    fn extend_span_fails_when_no_match() {
        let db = Database::open(":memory:").unwrap();
        let s1 = db.insert_snapshot("root", "20260220T0300", "nvme", "/snap1").unwrap();
        let s3 = db.insert_snapshot("root", "20260222T0300", "nvme", "/snap3").unwrap();
        let fid = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        db.insert_span(fid, s1, s1).unwrap();
        // s3 is not s1+1, so extend from s1 to s3 would only match if last_snap=s1
        // Actually this WILL match because we just check last_snap = prev_snap_id
        // The gap detection is the caller's responsibility. Here we test that
        // extend_span fails when last_snap doesn't match prev_snap_id.
        let extended = db.extend_span(fid, s3, s3).unwrap(); // no span ending at s3
        assert!(!extended);
    }
}
```

**Step 2: Run, verify fail**

Run: `cargo test --manifest-path indexer/Cargo.toml -- db::tests`
Expected: FAIL — methods don't exist.

**Step 3: Implement**

Add to `Database` impl:

```rust
pub fn upsert_file(&self, path: &str, name: &str, size: i64, mtime: i64, file_type: i32) -> SqlResult<i64> {
    // Try to get existing file
    if let Some(existing) = self.get_file(path)? {
        if existing.size != size || existing.mtime != mtime {
            self.conn.execute(
                "UPDATE files SET size = ?1, mtime = ?2 WHERE id = ?3",
                rusqlite::params![size, mtime, existing.id],
            )?;
        }
        return Ok(existing.id);
    }
    self.conn.execute(
        "INSERT INTO files (path, name, size, mtime, type) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![path, name, size, mtime, file_type],
    )?;
    Ok(self.conn.last_insert_rowid())
}

pub fn get_file(&self, path: &str) -> SqlResult<Option<FileRecord>> {
    let mut stmt = self.conn.prepare(
        "SELECT id, path, name, size, mtime, type FROM files WHERE path = ?1"
    )?;
    let mut rows = stmt.query_map([path], |row| {
        Ok(FileRecord {
            id: row.get(0)?,
            path: row.get(1)?,
            name: row.get(2)?,
            size: row.get(3)?,
            mtime: row.get(4)?,
            file_type: row.get(5)?,
        })
    })?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn insert_span(&self, file_id: i64, first_snap: i64, last_snap: i64) -> SqlResult<()> {
    self.conn.execute(
        "INSERT INTO spans (file_id, first_snap, last_snap) VALUES (?1, ?2, ?3)",
        rusqlite::params![file_id, first_snap, last_snap],
    )?;
    Ok(())
}

pub fn extend_span(&self, file_id: i64, prev_snap_id: i64, new_snap_id: i64) -> SqlResult<bool> {
    let rows = self.conn.execute(
        "UPDATE spans SET last_snap = ?1 WHERE file_id = ?2 AND last_snap = ?3",
        rusqlite::params![new_snap_id, file_id, prev_snap_id],
    )?;
    Ok(rows > 0)
}
```

**Step 4: Run, verify pass**

Run: `cargo test --manifest-path indexer/Cargo.toml -- db::tests`
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add indexer/src/db.rs
git commit -m "feat(indexer): file CRUD and span operations"
```

---

## Task 5: Database layer — FTS5 search

**Files:**
- Modify: `indexer/src/db.rs`

**Step 1: Write failing tests**

```rust
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    pub size: i64,
    pub mtime: i64,
    pub first_snap: String,
    pub last_snap: String,
}

#[cfg(test)]
mod tests {
    // ... existing tests ...

    fn setup_search_db() -> Database {
        let db = Database::open(":memory:").unwrap();
        let s1 = db.insert_snapshot("root", "20260220T0300", "nvme", "/snap1").unwrap();
        let f1 = db.upsert_file("docs/report.pdf", "report.pdf", 1000, 100, 0).unwrap();
        let f2 = db.upsert_file("photos/photo.jpg", "photo.jpg", 2000, 200, 0).unwrap();
        let f3 = db.upsert_file("docs/annual-report.docx", "annual-report.docx", 3000, 300, 0).unwrap();
        db.insert_span(f1, s1, s1).unwrap();
        db.insert_span(f2, s1, s1).unwrap();
        db.insert_span(f3, s1, s1).unwrap();
        db
    }

    #[test]
    fn fts5_search_by_name() {
        let db = setup_search_db();
        let results = db.search("report", 50).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn fts5_search_by_path() {
        let db = setup_search_db();
        let results = db.search("photos", 50).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "photo.jpg");
    }

    #[test]
    fn fts5_prefix_search() {
        let db = setup_search_db();
        let results = db.search("rep*", 50).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn fts5_no_results() {
        let db = setup_search_db();
        let results = db.search("nonexistent", 50).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_with_snapshot_info() {
        let db = setup_search_db();
        let results = db.search("report.pdf", 50).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].first_snap, "root.20260220T0300");
        assert_eq!(results[0].last_snap, "root.20260220T0300");
    }
}
```

**Step 2: Run, verify fail**

Run: `cargo test --manifest-path indexer/Cargo.toml -- db::tests`
Expected: FAIL — `search()` method doesn't exist.

**Step 3: Implement**

```rust
pub fn search(&self, query: &str, limit: i64) -> SqlResult<Vec<SearchResult>> {
    let mut stmt = self.conn.prepare(
        "SELECT f.path, f.name, f.size, f.mtime,
                s1.name || '.' || s1.ts AS first_snap,
                s2.name || '.' || s2.ts AS last_snap
         FROM files_fts
         JOIN files f ON f.id = files_fts.rowid
         JOIN spans sp ON sp.file_id = f.id
         JOIN snapshots s1 ON s1.id = sp.first_snap
         JOIN snapshots s2 ON s2.id = sp.last_snap
         WHERE files_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2"
    )?;
    let rows = stmt.query_map(rusqlite::params![query, limit], |row| {
        Ok(SearchResult {
            path: row.get(0)?,
            name: row.get(1)?,
            size: row.get(2)?,
            mtime: row.get(3)?,
            first_snap: row.get(4)?,
            last_snap: row.get(5)?,
        })
    })?;
    rows.collect()
}
```

**Step 4: Run, verify pass**

Run: `cargo test --manifest-path indexer/Cargo.toml -- db::tests`
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add indexer/src/db.rs
git commit -m "feat(indexer): FTS5 search with snapshot range info"
```

---

## Task 6: Scanner — filesystem walker

**Files:**
- Modify: `indexer/src/scanner.rs`

**Step 1: Write failing tests**

```rust
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,     // relative path within snapshot
    pub name: String,     // basename
    pub size: i64,
    pub mtime: i64,
    pub file_type: i32,   // 0=regular, 1=directory, 2=symlink, 3=other
}

pub struct ScanResult {
    pub entries: Vec<FileEntry>,
    pub errors: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;

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
        assert_eq!(entry.path, "test.txt"); // relative path
        assert_eq!(entry.file_type, 0); // regular file
    }

    #[test]
    fn identifies_symlinks() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("real.txt"), "content").unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("real.txt"),
            tmp.path().join("link.txt")
        ).unwrap();
        let result = scan_directory(tmp.path());
        let link = result.entries.iter().find(|e| e.name == "link.txt").unwrap();
        assert_eq!(link.file_type, 2); // symlink
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
```

**Step 2: Run, verify fail**

Run: `cargo test --manifest-path indexer/Cargo.toml -- scanner::tests`
Expected: FAIL — `scan_directory` function doesn't exist.

**Step 3: Implement**

```rust
use walkdir::WalkDir;
use std::os::unix::fs::MetadataExt;

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

                // For symlinks, use symlink metadata; for others, use regular metadata
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
```

**Step 4: Run, verify pass**

Run: `cargo test --manifest-path indexer/Cargo.toml -- scanner::tests`
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add indexer/src/scanner.rs
git commit -m "feat(indexer): filesystem scanner with walkdir and soft-fail errors"
```

---

## Task 7: Indexer — snapshot discovery

**Files:**
- Modify: `indexer/src/indexer.rs`

**Step 1: Write failing tests**

```rust
use std::path::PathBuf;
use crate::db::Database;

#[derive(Debug, Clone)]
pub struct DiscoveredSnapshot {
    pub name: String,       // "root"
    pub ts: String,         // "20260221T0304"
    pub source: String,     // "nvme"
    pub path: PathBuf,      // full path to snapshot dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;

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
        db.insert_snapshot("root", "20260220T0300", "nvme", &path1.to_string_lossy()).unwrap();
        let snaps = discover_snapshots(tmp.path(), &db).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].ts, "20260221T0300");
    }

    #[test]
    fn discovers_sources() {
        let tmp = TempDir::new().unwrap();
        make_snap_dirs(&tmp, &[
            "nvme/root.20260221T0300",
            "ssd/opt.20260221T0300",
            "projects/claude-projects.20260221T0300",
        ]);
        let db = Database::open(":memory:").unwrap();
        let snaps = discover_snapshots(tmp.path(), &db).unwrap();
        let sources: Vec<&str> = snaps.iter().map(|s| s.source.as_str()).collect();
        assert!(sources.contains(&"nvme"));
        assert!(sources.contains(&"ssd"));
        assert!(sources.contains(&"projects"));
    }
}
```

**Step 2: Run, verify fail**

Run: `cargo test --manifest-path indexer/Cargo.toml -- indexer::tests`
Expected: FAIL — functions don't exist.

**Step 3: Implement**

```rust
use regex::Regex;
use std::fs;

pub fn parse_snapshot_dirname(dirname: &str) -> Option<(String, String)> {
    let re = Regex::new(r"^(.+)\.(\d{8}T\d{4,6})$").unwrap();
    re.captures(dirname).map(|caps| {
        (caps[1].to_string(), caps[2].to_string())
    })
}

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
```

**Step 4: Run, verify pass**

Run: `cargo test --manifest-path indexer/Cargo.toml -- indexer::tests`
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add indexer/src/indexer.rs
git commit -m "feat(indexer): snapshot discovery from backup target"
```

---

## Task 8: Indexer — index a snapshot (span logic)

**Files:**
- Modify: `indexer/src/indexer.rs`

**Step 1: Write failing tests**

```rust
#[derive(Debug)]
pub struct IndexResult {
    pub snapshot_id: i64,
    pub files_total: usize,
    pub files_new: usize,
    pub files_extended: usize,
    pub files_changed: usize,
    pub scan_errors: usize,
}

#[cfg(test)]
mod tests {
    // ... existing tests ...
    use std::fs;
    use std::os::unix::fs::OpenOptionsExt;

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
            name: "root".into(), ts: "20260220T0300".into(),
            source: "nvme".into(), path: snap_dir,
        };
        let result = index_snapshot(&db, &snap, None).unwrap();
        assert_eq!(result.files_total, 4); // 3 files + 1 dir
        assert_eq!(result.files_new, 4);
        assert_eq!(result.files_extended, 0);
    }

    #[test]
    fn index_extends_spans_for_unchanged_files() {
        let tmp = TempDir::new().unwrap();
        let snap1 = tmp.path().join("nvme/root.20260220T0300");
        let snap2 = tmp.path().join("nvme/root.20260221T0300");
        write_file(&snap1.join("a.txt"), b"same");
        // Copy snap1 to snap2 (preserving timestamps)
        fs::create_dir_all(&snap2).unwrap();
        let src = snap1.join("a.txt");
        let dst = snap2.join("a.txt");
        fs::copy(&src, &dst).unwrap();
        // Preserve mtime
        let meta = fs::metadata(&src).unwrap();
        filetime::set_file_mtime(&dst, filetime::FileTime::from_last_modification_time(&meta)).unwrap();

        let db = Database::open(":memory:").unwrap();
        let ds1 = DiscoveredSnapshot {
            name: "root".into(), ts: "20260220T0300".into(),
            source: "nvme".into(), path: snap1,
        };
        let r1 = index_snapshot(&db, &ds1, None).unwrap();
        let ds2 = DiscoveredSnapshot {
            name: "root".into(), ts: "20260221T0300".into(),
            source: "nvme".into(), path: snap2,
        };
        let r2 = index_snapshot(&db, &ds2, Some(r1.snapshot_id)).unwrap();
        assert_eq!(r2.files_extended, 1);
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
        filetime::set_file_mtime(&snap2.join("a.txt"), filetime::FileTime::from_last_modification_time(&meta)).unwrap();

        let db = Database::open(":memory:").unwrap();
        let ds1 = DiscoveredSnapshot {
            name: "root".into(), ts: "20260220T0300".into(),
            source: "nvme".into(), path: snap1,
        };
        let r1 = index_snapshot(&db, &ds1, None).unwrap();
        let ds2 = DiscoveredSnapshot {
            name: "root".into(), ts: "20260221T0300".into(),
            source: "nvme".into(), path: snap2,
        };
        let r2 = index_snapshot(&db, &ds2, Some(r1.snapshot_id)).unwrap();
        assert_eq!(r2.files_new, 1);
        assert_eq!(r2.files_extended, 1);
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
            name: "root".into(), ts: "20260220T0300".into(),
            source: "nvme".into(), path: snap1,
        };
        let r1 = index_snapshot(&db, &ds1, None).unwrap();
        let ds2 = DiscoveredSnapshot {
            name: "root".into(), ts: "20260221T0300".into(),
            source: "nvme".into(), path: snap2,
        };
        let r2 = index_snapshot(&db, &ds2, Some(r1.snapshot_id)).unwrap();
        assert_eq!(r2.files_changed, 1);
    }
}
```

Note: Add `filetime = "0.2"` to `[dev-dependencies]` in `Cargo.toml` for mtime preservation in tests.

**Step 2: Run, verify fail**

Run: `cargo test --manifest-path indexer/Cargo.toml -- indexer::tests`
Expected: FAIL — `index_snapshot` doesn't exist.

**Step 3: Implement**

```rust
use crate::scanner::{scan_directory, FileEntry};
use std::collections::HashMap;

pub fn index_snapshot(
    db: &Database,
    snap: &DiscoveredSnapshot,
    prev_snap_id: Option<i64>,
) -> Result<IndexResult, Box<dyn std::error::Error>> {
    let scan = scan_directory(&snap.path);

    let tx = db.conn.unchecked_transaction()?;

    let snap_id = db.insert_snapshot(&snap.name, &snap.ts, &snap.source, &snap.path.to_string_lossy())?;

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
        let file_id = db.upsert_file(&entry.path, &entry.name, entry.size, entry.mtime, entry.file_type)?;

        let mut extended = false;
        if let Some(prev_id) = prev_snap_id {
            if let Some(prev_file) = prev_files.get(&entry.path) {
                if prev_file.size == entry.size && prev_file.mtime == entry.mtime {
                    extended = db.extend_span(file_id, prev_id, snap_id)?;
                    if extended {
                        files_extended += 1;
                    }
                } else {
                    files_changed += 1;
                }
            }
        }

        if !extended {
            if prev_files.get(&entry.path).is_none() || files_changed > 0 && prev_files.get(&entry.path).map(|f| f.size != entry.size || f.mtime != entry.mtime).unwrap_or(false) {
                // New file or changed file — either way, insert new span
            }
            if !extended {
                db.insert_span(file_id, snap_id, snap_id)?;
                if prev_snap_id.is_some() && !prev_files.contains_key(&entry.path) {
                    files_new += 1;
                } else if prev_snap_id.is_none() {
                    files_new += 1;
                }
            }
        }
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
```

Note: Also add `get_files_in_snapshot(snap_id)` to the `Database` impl — this queries spans joined with files to return all files present in a given snapshot.

```rust
// Add to db.rs Database impl
pub fn get_files_in_snapshot(&self, snap_id: i64) -> SqlResult<Vec<FileRecord>> {
    let mut stmt = self.conn.prepare(
        "SELECT f.id, f.path, f.name, f.size, f.mtime, f.type
         FROM files f
         JOIN spans s ON s.file_id = f.id
         WHERE s.first_snap <= ?1 AND s.last_snap >= ?1"
    )?;
    let rows = stmt.query_map([snap_id], |row| {
        Ok(FileRecord {
            id: row.get(0)?,
            path: row.get(1)?,
            name: row.get(2)?,
            size: row.get(3)?,
            mtime: row.get(4)?,
            file_type: row.get(5)?,
        })
    })?;
    rows.collect()
}
```

**Step 4: Run, verify pass**

Run: `cargo test --manifest-path indexer/Cargo.toml -- indexer::tests`
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add indexer/
git commit -m "feat(indexer): index snapshot with span-based deduplication"
```

---

## Task 9: Indexer — walk command (orchestrate full indexing run)

**Files:**
- Modify: `indexer/src/indexer.rs`

**Step 1: Write failing tests**

```rust
#[derive(Debug)]
pub struct WalkResult {
    pub snapshots_discovered: usize,
    pub snapshots_indexed: usize,
    pub snapshots_skipped: usize,
    pub results: Vec<IndexResult>,
}

#[cfg(test)]
mod tests {
    // ... existing tests ...

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
        // First walk indexes both
        let r1 = walk(tmp.path(), &db).unwrap();
        assert_eq!(r1.snapshots_indexed, 2);
        // Second walk skips both
        let r2 = walk(tmp.path(), &db).unwrap();
        assert_eq!(r2.snapshots_indexed, 0);
        assert_eq!(r2.snapshots_skipped, 2);
    }

    #[test]
    fn walk_orders_by_timestamp() {
        let tmp = TempDir::new().unwrap();
        // Create in reverse order
        make_snap_dirs(&tmp, &["nvme/root.20260222T0300", "nvme/root.20260220T0300", "nvme/root.20260221T0300"]);
        for d in &["20260220T0300", "20260221T0300", "20260222T0300"] {
            write_file(&tmp.path().join(format!("nvme/root.{}/a.txt", d)), b"a");
        }

        let db = Database::open(":memory:").unwrap();
        let result = walk(tmp.path(), &db).unwrap();
        assert_eq!(result.snapshots_indexed, 3);
        // Verify chronological order via snapshot IDs (ascending)
        let snaps = db.list_snapshots().unwrap();
        assert!(snaps[0].ts < snaps[1].ts);
        assert!(snaps[1].ts < snaps[2].ts);
    }
}
```

**Step 2: Run, verify fail**

Run: `cargo test --manifest-path indexer/Cargo.toml -- indexer::tests::walk`
Expected: FAIL — `walk` doesn't exist.

**Step 3: Implement**

```rust
pub fn walk(
    target_root: &std::path::Path,
    db: &Database,
) -> Result<WalkResult, Box<dyn std::error::Error>> {
    let discovered = discover_snapshots(target_root, db)?;
    let total_on_disk = discovered.len();

    // Group by (source, name) to find predecessors
    let mut results = Vec::new();
    let mut indexed_count = 0usize;

    // Track latest indexed snapshot per (source, name) for predecessor lookup
    let mut latest_snap_id: HashMap<(String, String), i64> = HashMap::new();

    // Pre-populate from already-indexed snapshots in DB
    for snap in db.list_snapshots()? {
        let key = (snap.source.clone(), snap.name.clone());
        match latest_snap_id.get(&key) {
            Some(&existing_id) => {
                // Keep the one with the latest ts
                let existing = db.get_snapshot_by_id(existing_id)?;
                if snap.ts > existing.ts {
                    latest_snap_id.insert(key, snap.id);
                }
            }
            None => { latest_snap_id.insert(key, snap.id); }
        }
    }

    for snap in &discovered {
        let key = (snap.source.clone(), snap.name.clone());
        let prev_id = latest_snap_id.get(&key).copied();

        let result = index_snapshot(db, snap, prev_id)?;
        latest_snap_id.insert(key, result.snapshot_id);
        results.push(result);
        indexed_count += 1;
    }

    // Count already-indexed snapshots on disk
    let total_in_db = db.list_snapshots()?.len();
    let skipped = total_in_db - indexed_count;

    Ok(WalkResult {
        snapshots_discovered: total_on_disk + skipped,
        snapshots_indexed: indexed_count,
        snapshots_skipped: skipped,
        results,
    })
}
```

Note: Add `get_snapshot_by_id(id)` helper to `Database`.

**Step 4: Run, verify pass**

Run: `cargo test --manifest-path indexer/Cargo.toml -- indexer::tests::walk`
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add indexer/
git commit -m "feat(indexer): walk command orchestrates full indexing run"
```

---

## Task 10: CLI — clap subcommands

**Files:**
- Modify: `indexer/src/main.rs`

**Step 1: Implement CLI with clap derive**

```rust
use clap::{Parser, Subcommand};
use das_index::db::Database;
use das_index::indexer;
use std::path::PathBuf;

const DEFAULT_DB: &str = "/var/lib/das-backup/backup-index.db";

#[derive(Parser)]
#[command(name = "das-index", about = "Content indexer for DAS backup snapshots")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index all new snapshots on a backup target
    Walk {
        /// Path to backup target mount point
        target: PathBuf,
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
    },
    /// Full-text search across indexed files
    Search {
        /// FTS5 search query (supports prefix: "report*")
        query: String,
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        /// Maximum results to return
        #[arg(long, default_value = "50")]
        limit: i64,
    },
    /// List files in a specific snapshot
    List {
        /// Snapshot path or name.timestamp pattern
        snapshot: String,
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
    },
    /// Show database statistics
    Info {
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Walk { target, db } => {
            let database = Database::open(&db)?;
            let result = indexer::walk(&target, &database)?;
            println!("Discovered: {} snapshots", result.snapshots_discovered);
            println!("Indexed:    {} new", result.snapshots_indexed);
            println!("Skipped:    {} already indexed", result.snapshots_skipped);
            for r in &result.results {
                println!("  {} files ({} new, {} extended, {} changed, {} errors)",
                    r.files_total, r.files_new, r.files_extended, r.files_changed, r.scan_errors);
            }
        }
        Commands::Search { query, db, limit } => {
            let database = Database::open(&db)?;
            let results = database.search(&query, limit)?;
            if results.is_empty() {
                println!("No matches for '{}'", query);
            } else {
                for r in &results {
                    println!("{}\t{}\t{}\t{}\t{}", r.path, r.size, r.mtime, r.first_snap, r.last_snap);
                }
                println!("({} results)", results.len());
            }
        }
        Commands::List { snapshot, db } => {
            let database = Database::open(&db)?;
            let files = database.list_files_in_snapshot(&snapshot)?;
            for f in &files {
                println!("{}", f.path);
            }
            println!("({} files)", files.len());
        }
        Commands::Info { db } => {
            let database = Database::open(&db)?;
            let stats = database.get_stats()?;
            println!("Snapshots:  {}", stats.snapshot_count);
            println!("Files:      {}", stats.file_count);
            println!("Spans:      {}", stats.span_count);
            println!("DB size:    {} bytes", stats.db_size);
        }
    }

    Ok(())
}
```

Note: Add `list_files_in_snapshot(snapshot_pattern)` and `get_stats()` methods to `Database`.

```rust
// In db.rs
pub struct DbStats {
    pub snapshot_count: i64,
    pub file_count: i64,
    pub span_count: i64,
    pub db_size: i64,
}

pub fn get_stats(&self) -> SqlResult<DbStats> {
    Ok(DbStats {
        snapshot_count: self.conn.query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))?,
        file_count: self.conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?,
        span_count: self.conn.query_row("SELECT COUNT(*) FROM spans", [], |r| r.get(0))?,
        db_size: self.conn.query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| r.get(0))?,
    })
}

pub fn list_files_in_snapshot(&self, snapshot_pattern: &str) -> SqlResult<Vec<FileRecord>> {
    // Find snapshot by path or name.ts pattern
    let snap = self.conn.query_row(
        "SELECT id FROM snapshots WHERE path = ?1 OR (name || '.' || ts) = ?1",
        [snapshot_pattern],
        |row| row.get::<_, i64>(0),
    )?;
    self.get_files_in_snapshot(snap)
}
```

**Step 2: Build and test manually**

Run: `cargo build --manifest-path indexer/Cargo.toml`
Run: `./indexer/target/debug/das-index info --db /tmp/test-index.db`
Run: `./indexer/target/debug/das-index search "test" --db /tmp/test-index.db`
Expected: Builds, runs, shows empty stats / no results.

**Step 3: Commit**

```bash
git add indexer/
git commit -m "feat(indexer): CLI with walk, search, list, info subcommands"
```

---

## Task 11: Integration with backup-run.sh

**Files:**
- Modify: `scripts/backup-run.sh`

**Step 1: Add indexing call after btrbk completes**

Add a new function `run_indexer()` to backup-run.sh, called after `run_btrbk()` and `capture_usage "after"`:

```bash
run_indexer() {
    local indexer="/hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer/target/release/das-index"
    local db="/var/lib/das-backup/backup-index.db"

    if [[ ! -x "$indexer" ]]; then
        log_warn "Content indexer not built -- skipping (build with: cargo build --release --manifest-path indexer/Cargo.toml)"
        record_op "indexer" "SKIP" "binary not found"
        return
    fi

    log_info "Running content indexer..."
    local indexer_output
    if indexer_output=$("$indexer" walk "$MOUNT_BACKUP" --db "$db" 2>&1); then
        record_op "indexer" "OK"
        log_info "  $indexer_output"
    else
        log_warn "Content indexer failed (non-fatal)"
        record_op "indexer" "FAIL" "exit code $?"
    fi
}
```

Add `Indexer` line to the email report BACKUP OPERATIONS section.

**Step 2: Test manually** (with DAS not mounted — verify skip message)

Run: `sudo bash scripts/backup-run.sh --dryrun 2>&1 | grep -i indexer`
Expected: Shows "Content indexer not built -- skipping" or similar.

**Step 3: Commit**

```bash
git add scripts/backup-run.sh
git commit -m "feat: integrate content indexer into backup-run.sh"
```

---

## Task 12: Final verification and release

**Step 1: Run full test suite**

```bash
cargo test --manifest-path indexer/Cargo.toml
```

Expected: All tests pass.

**Step 2: Build release binary**

```bash
cargo build --release --manifest-path indexer/Cargo.toml
ls -la indexer/target/release/das-index
```

Expected: Optimized binary exists, reasonable size.

**Step 3: Run clippy and format check**

```bash
cargo clippy --manifest-path indexer/Cargo.toml -- -D warnings
cargo fmt --manifest-path indexer/Cargo.toml -- --check
```

Expected: No warnings, properly formatted.

**Step 4: Update CHANGELOG.md and README.md**

Bump version to 0.3.0. Document:
- New `das-index` Rust CLI tool
- SQLite FTS5 with span-based storage
- Integration with backup-run.sh

**Step 5: Update CLAUDE.md**

Add Rust to the tech stack line: "C++20/Rust — Qt6 6.10.2, KF6 6.23.0, CMake 4.2.3, Cargo/Rust 1.93, SQLite 3.51.2 FTS5"

**Step 6: Commit and push**

```bash
git add -A
git commit -m "feat(indexer): complete Rust content indexer with FTS5 search and span storage"
git push origin main
```
