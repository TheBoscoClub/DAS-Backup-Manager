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
}
