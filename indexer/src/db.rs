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
    INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
"#;

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub id: i64,
    pub name: String,
    pub ts: String,
    pub source: String,
    pub path: String,
    pub indexed_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub size: i64,
    pub mtime: i64,
    pub file_type: i32,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    pub size: i64,
    pub mtime: i64,
    pub first_snap: String,
    pub last_snap: String,
}

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

    pub fn get_snapshot_by_id(&self, id: i64) -> SqlResult<Snapshot> {
        self.conn.query_row(
            "SELECT id, name, ts, source, path, indexed_at FROM snapshots WHERE id = ?1",
            [id],
            |row| {
                Ok(Snapshot {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    ts: row.get(2)?,
                    source: row.get(3)?,
                    path: row.get(4)?,
                    indexed_at: row.get(5)?,
                })
            },
        )
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

    pub fn upsert_file(&self, path: &str, name: &str, size: i64, mtime: i64, file_type: i32) -> SqlResult<i64> {
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

    pub fn get_files_in_snapshot(&self, snap_id: i64) -> SqlResult<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.path, f.name, f.size, f.mtime, f.type
             FROM files f
             JOIN spans s ON s.file_id = f.id
             WHERE s.first_snap <= ?1 AND s.last_snap >= ?1",
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

    pub fn search(&self, query: &str, limit: i64) -> SqlResult<Vec<SearchResult>> {
        // Wrap bare terms in quotes so FTS5 treats punctuation (dots, hyphens) as literals.
        // Preserve explicit FTS5 syntax: prefix wildcard (*), column filters (:), boolean ops.
        let fts_query = if query.contains('*') || query.contains(':') || query.contains('"') {
            query.to_string()
        } else {
            format!("\"{}\"", query)
        };
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
        let rows = stmt.query_map(rusqlite::params![fts_query, limit], |row| {
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
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = self.conn.execute_batch("PRAGMA optimize;");
    }
}

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
        // :memory: databases may report "memory" instead of "wal"
        // For file-based DBs this would be "wal"
        // Accept either for in-memory tests
        assert!(mode == "wal" || mode == "memory");
    }

    #[test]
    fn foreign_keys_enabled() {
        let db = Database::open(":memory:").unwrap();
        let fk: i64 = db.conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

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
        assert_eq!(id1, id2);
    }

    #[test]
    fn upsert_file_changed() {
        let db = Database::open(":memory:").unwrap();
        let id1 = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        let id2 = db.upsert_file("a.txt", "a.txt", 200, 2000, 0).unwrap();
        assert_eq!(id1, id2);
        let f = db.get_file("a.txt").unwrap().unwrap();
        assert_eq!(f.size, 200);
        assert_eq!(f.mtime, 2000);
    }

    #[test]
    fn insert_span() {
        let db = Database::open(":memory:").unwrap();
        let snap_id = db.insert_snapshot("root", "20260221T0304", "nvme", "/snap1").unwrap();
        let file_id = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        db.insert_span(file_id, snap_id, snap_id).unwrap();
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
        let extended = db.extend_span(fid, s3, s3).unwrap();
        assert!(!extended);
    }

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
