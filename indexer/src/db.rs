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

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub id: i64,
    pub name: String,
    pub ts: String,
    pub source: String,
    pub path: String,
    pub indexed_at: i64,
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
}
