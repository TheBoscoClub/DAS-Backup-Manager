use rusqlite::{Connection, OptionalExtension, Result as SqlResult};
use std::path::Path;
use std::time::Duration;

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

-- Performance indexes for common query patterns
CREATE INDEX IF NOT EXISTS idx_snapshots_source_name ON snapshots(source, name);
CREATE INDEX IF NOT EXISTS idx_snapshots_ts ON snapshots(ts);
CREATE INDEX IF NOT EXISTS idx_spans_file_id ON spans(file_id);
CREATE INDEX IF NOT EXISTS idx_files_name ON files(name);

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

-- Backup run history
CREATE TABLE IF NOT EXISTS backup_runs (
    id              INTEGER PRIMARY KEY,
    timestamp       INTEGER NOT NULL,
    success         INTEGER NOT NULL DEFAULT 0,
    mode            TEXT NOT NULL DEFAULT 'incremental',
    snaps_created   INTEGER NOT NULL DEFAULT 0,
    snaps_sent      INTEGER NOT NULL DEFAULT 0,
    bytes_sent      INTEGER NOT NULL DEFAULT 0,
    duration_secs   INTEGER NOT NULL DEFAULT 0,
    errors          TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_backup_runs_ts ON backup_runs(timestamp);

-- Target disk usage tracking
CREATE TABLE IF NOT EXISTS target_usage (
    id              INTEGER PRIMARY KEY,
    timestamp       INTEGER NOT NULL,
    target_label    TEXT NOT NULL,
    total_bytes     INTEGER NOT NULL DEFAULT 0,
    used_bytes      INTEGER NOT NULL DEFAULT 0,
    snapshot_count  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_target_usage_label_ts ON target_usage(target_label, timestamp);
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
pub struct DbStats {
    pub snapshot_count: i64,
    pub file_count: i64,
    pub span_count: i64,
    pub db_size: i64,
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
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Reduce cold-cache latency on multi-GB indices: mmap a generous
        // 4 GiB window so SQLite reads via the kernel page cache (shared
        // with the next opener) and bump the per-connection page cache
        // to 256 MiB so b-tree walks don't thrash on small queries.
        // negative cache_size = KiB, so -262144 = 256 MiB.
        // See DAS-Backup-Manager-aem.
        conn.pragma_update(None, "mmap_size", 4_294_967_296i64)?;
        conn.pragma_update(None, "cache_size", -262_144i64)?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Database { conn })
    }

    pub fn insert_snapshot(
        &self,
        name: &str,
        ts: &str,
        source: &str,
        path: &str,
    ) -> SqlResult<i64> {
        let indexed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
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

    /// Check if a snapshot with the given (source, name, ts) triple already
    /// exists in the database.  This is mount-path-agnostic — it prevents
    /// duplicate indexing when the same snapshot is accessed via different
    /// mount points (e.g. bind mount `/mnt/backup-22tb` vs udisks2
    /// `/run/media/bosco/das-backup-22tb`).
    pub fn snapshot_exists_by_key(&self, source: &str, name: &str, ts: &str) -> SqlResult<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM snapshots WHERE source = ?1 AND name = ?2 AND ts = ?3",
            rusqlite::params![source, name, ts],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn get_snapshot(&self, path: &str) -> SqlResult<Option<Snapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, ts, source, path, indexed_at FROM snapshots WHERE path = ?1",
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

    /// Get just the filesystem path for a snapshot by ID.
    pub fn snapshot_path_by_id(&self, id: i64) -> SqlResult<Option<String>> {
        self.conn
            .query_row("SELECT path FROM snapshots WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
    }

    pub fn list_snapshots(&self) -> SqlResult<Vec<Snapshot>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, ts, source, path, indexed_at FROM snapshots ORDER BY ts")?;
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

    pub fn upsert_file(
        &self,
        path: &str,
        name: &str,
        size: i64,
        mtime: i64,
        file_type: i32,
    ) -> SqlResult<i64> {
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
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, name, size, mtime, type FROM files WHERE path = ?1")?;
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

    pub fn extend_span(
        &self,
        file_id: i64,
        prev_snap_id: i64,
        new_snap_id: i64,
    ) -> SqlResult<bool> {
        let rows = self.conn.execute(
            "UPDATE spans SET last_snap = ?1 WHERE file_id = ?2 AND last_snap = ?3",
            rusqlite::params![new_snap_id, file_id, prev_snap_id],
        )?;
        Ok(rows > 0)
    }

    /// Files present in `snap_id`, resolved from the span index.
    ///
    /// # Why the `snapshots` self-join is load-bearing
    ///
    /// A span is `(file_id, first_snap, last_snap)` — an *id range*. Snapshot ids
    /// are allocated globally in indexing order, so every source's snapshots are
    /// interleaved in id space. Matching on the range alone therefore claims a file
    /// is present in every snapshot whose id falls in the window, **across unrelated
    /// subvolumes**: before `bd DAS-Backup-Manager-5uq` this reported a
    /// `cloudflare-manager` project snapshot as containing Stephen King audiobooks,
    /// with a file count of 3,637,016 for a project of a few hundred files.
    ///
    /// Constraining `(name, source)` of the span's `first_snap` to equal the target
    /// snapshot's restores correctness. Deriving the series from `first_snap` is
    /// sound because spans are series-coherent by construction — verified against
    /// the production index, 0 of 85,038,167 spans had endpoints in different series.
    pub fn get_files_in_snapshot(&self, snap_id: i64) -> SqlResult<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.path, f.name, f.size, f.mtime, f.type
             FROM files f
             JOIN spans s ON s.file_id = f.id
             JOIN snapshots fs ON fs.id = s.first_snap
             JOIN snapshots tgt ON tgt.id = ?1
             WHERE s.first_snap <= ?1 AND s.last_snap >= ?1
               AND fs.name = tgt.name AND fs.source = tgt.source",
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

    /// Paginated version of `get_files_in_snapshot` with LIMIT and OFFSET.
    pub fn get_files_in_snapshot_paged(
        &self,
        snap_id: i64,
        limit: i64,
        offset: i64,
    ) -> SqlResult<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.path, f.name, f.size, f.mtime, f.type
             FROM files f
             JOIN spans s ON s.file_id = f.id
             JOIN snapshots fs ON fs.id = s.first_snap
             JOIN snapshots tgt ON tgt.id = ?1
             WHERE s.first_snap <= ?1 AND s.last_snap >= ?1
               AND fs.name = tgt.name AND fs.source = tgt.source
             ORDER BY f.path
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![snap_id, limit, offset], |row| {
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

    /// Count files in a snapshot (for pagination metadata).
    pub fn count_files_in_snapshot(&self, snap_id: i64) -> SqlResult<i64> {
        self.conn.query_row(
            "SELECT COUNT(DISTINCT f.id)
             FROM files f
             JOIN spans s ON s.file_id = f.id
             JOIN snapshots fs ON fs.id = s.first_snap
             JOIN snapshots tgt ON tgt.id = ?1
             WHERE s.first_snap <= ?1 AND s.last_snap >= ?1
               AND fs.name = tgt.name AND fs.source = tgt.source",
            [snap_id],
            |row| row.get(0),
        )
    }

    pub fn get_stats(&self) -> SqlResult<DbStats> {
        Ok(DbStats {
            snapshot_count: self
                .conn
                .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))?,
            file_count: self
                .conn
                .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?,
            span_count: self
                .conn
                .query_row("SELECT COUNT(*) FROM spans", [], |r| r.get(0))?,
            db_size: self.conn.query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |r| r.get(0),
            )?,
        })
    }

    pub fn list_files_in_snapshot(&self, snapshot_pattern: &str) -> SqlResult<Vec<FileRecord>> {
        let snap_id: i64 = self.conn.query_row(
            "SELECT id FROM snapshots WHERE path = ?1 OR (name || '.' || ts) = ?1 OR path LIKE '%/' || ?1",
            [snapshot_pattern],
            |row| row.get(0),
        )?;
        self.get_files_in_snapshot(snap_id)
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
             LIMIT ?2",
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

    // -----------------------------------------------------------------
    // Reconciliation (bd DAS-Backup-Manager-cu8)
    // -----------------------------------------------------------------

    /// Remove index rows for `doomed` snapshot ids, repairing spans that merely
    /// *touch* them rather than discarding presence information for snapshots
    /// that survive.
    ///
    /// # Span repair, and why whole-span deletion would be wrong
    ///
    /// A span is `(file_id, first_snap, last_snap)` — an id range covering every
    /// snapshot of one series in which the file was present. Retention deletes the
    /// OLDEST snapshots, which are overwhelmingly the `first_snap` end. Dropping any
    /// span that references a doomed id would therefore delete the file's presence
    /// across the whole surviving range too: prune one 2026-08-02 snapshot and the
    /// file silently disappears from every snapshot up to today.
    ///
    /// So endpoints are moved inward to the nearest surviving snapshot of the same
    /// series, and only spans with no surviving snapshot at all are deleted. Spans
    /// whose doomed ids fall strictly *between* the endpoints need no change — the
    /// id simply ceases to exist and can never be matched again.
    ///
    /// Ordering is load-bearing: survivor-less spans are deleted first (an `UPDATE`
    /// to a `NULL` endpoint would violate `NOT NULL`), endpoints are repaired while
    /// the doomed `snapshots` rows still exist to resolve each span's series, and
    /// only then are the snapshot rows themselves removed. Files left with no spans
    /// are deleted last; the existing `files_ad` trigger keeps `files_fts` in step.
    ///
    /// The whole pass is one transaction — a partial prune would leave spans
    /// pointing at snapshot ids that no longer resolve, which reads as an empty
    /// snapshot rather than an error.
    pub fn prune_snapshots(&self, doomed: &[i64]) -> SqlResult<crate::reconcile::PruneStats> {
        use crate::reconcile::PruneStats;

        if doomed.is_empty() {
            return Ok(PruneStats::default());
        }

        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS doomed_snaps(id INTEGER PRIMARY KEY);
             DELETE FROM doomed_snaps;",
        )?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO doomed_snaps(id) VALUES (?1)")?;
            for id in doomed {
                ins.execute([id])?;
            }
        }

        // (0) Refuse to prune an index that already violates its own foreign keys.
        //
        // `PRAGMA foreign_keys` is ON (see `Database::open`), so re-inserting a
        // repaired span re-validates it: a row whose `last_snap` was ALREADY
        // dangling is tolerated as legacy data but REJECTED on rewrite, aborting
        // the pass with SQLITE_CONSTRAINT_FOREIGNKEY after minutes of work.
        // Observed live 2026-08-24 against an index carrying 14,984,318 such rows
        // (`bd DAS-Backup-Manager-opd`).
        //
        // The check is deliberately scoped to the spans this pass would actually
        // rewrite rather than the whole table, so a healthy index pays a bounded
        // cost instead of a full 85M-row `PRAGMA foreign_key_check`.
        let dangling: i64 = tx.query_row(
            "SELECT COUNT(*) FROM spans s
             WHERE s.first_snap IN (SELECT id FROM doomed_snaps)
               AND NOT EXISTS (SELECT 1 FROM snapshots n WHERE n.id = s.last_snap)",
            [],
            |r| r.get(0),
        )?;
        if dangling > 0 {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                Some(format!(
                    "index has {dangling} span(s) referencing snapshots that do not exist; \
                     refusing to prune (see bd DAS-Backup-Manager-opd — rebuild the index)"
                )),
            ));
        }

        // (A) Spans with no surviving snapshot in their series/range at all.
        let spans_removed = tx.execute(
            "DELETE FROM spans
             WHERE (first_snap IN (SELECT id FROM doomed_snaps)
                 OR last_snap  IN (SELECT id FROM doomed_snaps))
               AND NOT EXISTS (
                   SELECT 1 FROM snapshots s2, snapshots ser
                   WHERE ser.id = spans.first_snap
                     AND s2.name = ser.name AND s2.source = ser.source
                     AND s2.id >= spans.first_snap AND s2.id <= spans.last_snap
                     AND s2.id NOT IN (SELECT id FROM doomed_snaps))",
            [],
        )?;

        // (B) Pull a doomed first_snap forward to the earliest survivor.
        //
        // This CANNOT be a plain UPDATE. spans is keyed (file_id, first_snap),
        // and moving a start forward can land it on a start another span of the
        // same file already occupies — 7,402,503 overlapping span pairs exist in
        // the production index, e.g. `.Trash-0` holding both (219,221) and
        // (220,220) in one series, where pruning 219 moves the first onto 220.
        // A direct UPDATE aborts the whole pass with SQLITE_CONSTRAINT_PRIMARYKEY
        // (1555); an earlier build did exactly that three minutes into a live run.
        //
        // So repairs are materialized first, then re-inserted grouped by their
        // destination key with ON CONFLICT DO UPDATE. Overlapping spans are
        // redundant by definition — (219,221) already covers (220,220) — so the
        // correct resolution is a merge to the union, which the GROUP BY handles
        // for repaired-vs-repaired and the upsert handles for repaired-vs-existing.
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS span_repair(
                 rid INTEGER PRIMARY KEY, file_id INTEGER, last_snap INTEGER, new_first INTEGER);
             DELETE FROM span_repair;",
        )?;
        tx.execute(
            "INSERT INTO span_repair(rid, file_id, last_snap, new_first)
             SELECT s.rowid, s.file_id, s.last_snap,
                    (SELECT MIN(s2.id) FROM snapshots s2, snapshots ser
                     WHERE ser.id = s.first_snap
                       AND s2.name = ser.name AND s2.source = ser.source
                       AND s2.id >= s.first_snap AND s2.id <= s.last_snap
                       AND s2.id NOT IN (SELECT id FROM doomed_snaps))
             FROM spans s
             WHERE s.first_snap IN (SELECT id FROM doomed_snaps)",
            [],
        )?;
        // Detach EVERY candidate, including any with no survivor. Step (A) should
        // already have removed those — a span with no survivor necessarily has a
        // doomed `last_snap`, which (A) matches — but detaching unconditionally
        // means a hypothetical NULL is simply dropped rather than left behind
        // pointing at a snapshot row that is about to disappear.
        let detached = tx.execute(
            "DELETE FROM spans WHERE rowid IN (SELECT rid FROM span_repair)",
            [],
        )?;
        // Accounting has to be derived, not read off the upsert: `INSERT ... ON
        // CONFLICT DO UPDATE` reports ONE change whether it inserted a fresh span
        // or absorbed an existing one, so it cannot distinguish "endpoint moved"
        // from "two spans became one".
        //
        //   groups   — distinct destinations, i.e. spans that survive the repair
        //   collapsed— candidates that merged into each other  (detached - groups)
        //   collided — destinations already occupied by a span that stays put
        let groups: i64 = tx.query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT file_id, new_first
                                   FROM span_repair WHERE new_first IS NOT NULL)",
            [],
            |r| r.get(0),
        )?;
        let collided: i64 = tx.query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT file_id, new_first
                                   FROM span_repair WHERE new_first IS NOT NULL) r
             WHERE EXISTS (SELECT 1 FROM spans s
                           WHERE s.file_id = r.file_id AND s.first_snap = r.new_first)",
            [],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO spans(file_id, first_snap, last_snap)
             SELECT file_id, new_first, MAX(last_snap)
             FROM span_repair WHERE new_first IS NOT NULL
             GROUP BY file_id, new_first
             ON CONFLICT(file_id, first_snap)
               DO UPDATE SET last_snap = MAX(last_snap, excluded.last_snap)",
            [],
        )?;
        let groups = groups as usize;
        let collided = collided as usize;
        let repaired_first = groups;
        let merged_away = detached.saturating_sub(groups) + collided;

        // (C) Pull a doomed last_snap back to the latest survivor.
        let repaired_last = tx.execute(
            "UPDATE spans SET last_snap = (
                 SELECT MAX(s2.id) FROM snapshots s2, snapshots ser
                 WHERE ser.id = spans.last_snap
                   AND s2.name = ser.name AND s2.source = ser.source
                   AND s2.id >= spans.first_snap AND s2.id <= spans.last_snap
                   AND s2.id NOT IN (SELECT id FROM doomed_snaps))
             WHERE last_snap IN (SELECT id FROM doomed_snaps)",
            [],
        )?;

        // (D) The snapshot rows themselves.
        let snapshots_removed = tx.execute(
            "DELETE FROM snapshots WHERE id IN (SELECT id FROM doomed_snaps)",
            [],
        )?;

        // (E) Files that no span references any more. files_ad maintains files_fts.
        let files_removed = tx.execute(
            "DELETE FROM files
             WHERE NOT EXISTS (SELECT 1 FROM spans WHERE spans.file_id = files.id)",
            [],
        )?;

        tx.execute_batch("DROP TABLE IF EXISTS doomed_snaps; DROP TABLE IF EXISTS span_repair;")?;
        tx.commit()?;

        Ok(PruneStats {
            snapshots_removed,
            spans_removed: spans_removed + merged_away,
            spans_repaired: repaired_first + repaired_last,
            files_removed,
        })
    }

    // -----------------------------------------------------------------
    // Backup run history
    // -----------------------------------------------------------------

    /// Record a completed backup run. Returns the new row ID.
    pub fn insert_backup_run(&self, run: &NewBackupRun<'_>) -> SqlResult<i64> {
        let errors_str = run.errors.join("\n");
        self.conn.execute(
            "INSERT INTO backup_runs (timestamp, success, mode, snaps_created, snaps_sent, bytes_sent, duration_secs, errors)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                run.timestamp,
                run.success as i32,
                run.mode,
                run.snaps_created as i64,
                run.snaps_sent as i64,
                run.bytes_sent as i64,
                run.duration_secs as i64,
                errors_str,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get the most recent backup runs, ordered newest first.
    pub fn get_backup_history(&self, limit: usize) -> SqlResult<Vec<BackupRunRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, success, mode, snaps_created, snaps_sent, bytes_sent, duration_secs, errors
             FROM backup_runs ORDER BY timestamp DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            let errors_str: String = row.get(8)?;
            let errors: Vec<String> = if errors_str.is_empty() {
                Vec::new()
            } else {
                errors_str.split('\n').map(|s| s.to_string()).collect()
            };
            Ok(BackupRunRecord {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                success: row.get::<_, i32>(2)? != 0,
                mode: row.get(3)?,
                snaps_created: row.get::<_, i64>(4)? as usize,
                snaps_sent: row.get::<_, i64>(5)? as usize,
                bytes_sent: row.get::<_, i64>(6)? as u64,
                duration_secs: row.get::<_, i64>(7)? as u64,
                errors,
            })
        })?;
        rows.collect()
    }

    // -----------------------------------------------------------------
    // Target usage tracking
    // -----------------------------------------------------------------

    /// Record a target disk usage snapshot.
    pub fn insert_target_usage(
        &self,
        timestamp: i64,
        target_label: &str,
        total_bytes: u64,
        used_bytes: u64,
        snapshot_count: usize,
    ) -> SqlResult<i64> {
        self.conn.execute(
            "INSERT INTO target_usage (timestamp, target_label, total_bytes, used_bytes, snapshot_count)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                timestamp,
                target_label,
                total_bytes as i64,
                used_bytes as i64,
                snapshot_count as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get usage history for a specific target over the last N days.
    pub fn get_target_usage_history(
        &self,
        target_label: &str,
        days: u32,
    ) -> SqlResult<Vec<TargetUsageRecord>> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs() as i64
            - (days as i64 * 86400);
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, target_label, total_bytes, used_bytes, snapshot_count
             FROM target_usage
             WHERE target_label = ?1 AND timestamp >= ?2
             ORDER BY timestamp ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![target_label, cutoff], |row| {
            Ok(TargetUsageRecord {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                target_label: row.get(2)?,
                total_bytes: row.get::<_, i64>(3)? as u64,
                used_bytes: row.get::<_, i64>(4)? as u64,
                snapshot_count: row.get::<_, i64>(5)? as usize,
            })
        })?;
        rows.collect()
    }
}

/// Input for recording a new backup run.
pub struct NewBackupRun<'a> {
    pub timestamp: i64,
    pub success: bool,
    pub mode: &'a str,
    pub snaps_created: usize,
    pub snaps_sent: usize,
    pub bytes_sent: u64,
    pub duration_secs: u64,
    pub errors: &'a [String],
}

/// A backup run record from the database.
#[derive(Debug, Clone)]
pub struct BackupRunRecord {
    pub id: i64,
    pub timestamp: i64,
    pub success: bool,
    pub mode: String,
    pub snaps_created: usize,
    pub snaps_sent: usize,
    pub bytes_sent: u64,
    pub duration_secs: u64,
    pub errors: Vec<String>,
}

/// A target usage record from the database.
#[derive(Debug, Clone)]
pub struct TargetUsageRecord {
    pub id: i64,
    pub timestamp: i64,
    pub target_label: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub snapshot_count: usize,
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = self.conn.execute_batch("PRAGMA optimize;");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // bd DAS-Backup-Manager-5uq — span reads must be scoped to one series
    // -----------------------------------------------------------------

    /// Two series whose snapshot ids interleave, exactly as production allocates
    /// them: every source is snapshotted in the same run, so ids alternate.
    fn interleaved_fixture() -> (Database, i64, i64, i64, i64) {
        let db = Database::open(":memory:").unwrap();
        let a1 = db
            .insert_snapshot(
                "projA",
                "20260101T0300",
                "projects",
                "/mnt/t/projects/projA.20260101T0300",
            )
            .unwrap();
        let b1 = db
            .insert_snapshot(
                "abooks",
                "20260101T0300",
                "audiobooks",
                "/mnt/t/audiobooks/abooks.20260101T0300",
            )
            .unwrap();
        let a2 = db
            .insert_snapshot(
                "projA",
                "20260102T0300",
                "projects",
                "/mnt/t/projects/projA.20260102T0300",
            )
            .unwrap();
        let b2 = db
            .insert_snapshot(
                "abooks",
                "20260102T0300",
                "audiobooks",
                "/mnt/t/audiobooks/abooks.20260102T0300",
            )
            .unwrap();
        assert!(a1 < b1 && b1 < a2 && a2 < b2, "fixture must interleave ids");
        (db, a1, b1, a2, b2)
    }

    #[test]
    fn files_in_snapshot_are_scoped_to_their_series() {
        // REGRESSION (5uq): before the fix this returned the projects file too,
        // because span (a1..a2) brackets b1 in raw id space. Live symptom was a
        // cloudflare-manager snapshot listing Stephen King audiobooks.
        let (db, a1, b1, a2, _b2) = interleaved_fixture();

        let proj = db.upsert_file("src/main.rs", "main.rs", 10, 0, 0).unwrap();
        db.insert_span(proj, a1, a2).unwrap();

        let abook = db
            .upsert_file("Library/book.opus", "book.opus", 20, 0, 0)
            .unwrap();
        db.insert_span(abook, b1, b1).unwrap();

        let listed = db.get_files_in_snapshot(b1).unwrap();
        let paths: Vec<&str> = listed.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["Library/book.opus"]);
        assert!(
            !paths.contains(&"src/main.rs"),
            "a projects-series span must never appear in an audiobooks snapshot"
        );
    }

    #[test]
    fn count_in_snapshot_is_scoped_to_its_series() {
        let (db, a1, b1, a2, _b2) = interleaved_fixture();
        let proj = db.upsert_file("src/main.rs", "main.rs", 10, 0, 0).unwrap();
        db.insert_span(proj, a1, a2).unwrap();
        // Pre-fix this counted 1 (the foreign projects file); correct answer is 0.
        assert_eq!(db.count_files_in_snapshot(b1).unwrap(), 0);
        assert_eq!(db.count_files_in_snapshot(a1).unwrap(), 1);
    }

    #[test]
    fn paged_listing_is_scoped_to_its_series() {
        let (db, a1, b1, a2, _b2) = interleaved_fixture();
        let proj = db.upsert_file("src/main.rs", "main.rs", 10, 0, 0).unwrap();
        db.insert_span(proj, a1, a2).unwrap();
        assert!(
            db.get_files_in_snapshot_paged(b1, 100, 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(db.get_files_in_snapshot_paged(a1, 100, 0).unwrap().len(), 1);
    }

    #[test]
    fn same_relative_path_in_two_series_stays_separated() {
        // files.path is UNIQUE and relative-within-snapshot, so both series share
        // ONE files row. Scoping must still keep their spans apart.
        let (db, a1, b1, a2, b2) = interleaved_fixture();
        let shared = db
            .upsert_file("packaging/PKGBUILD", "PKGBUILD", 1, 0, 0)
            .unwrap();
        db.insert_span(shared, a1, a2).unwrap();
        db.insert_span(shared, b1, b2).unwrap();
        assert_eq!(db.get_files_in_snapshot(a1).unwrap().len(), 1);
        assert_eq!(db.get_files_in_snapshot(b1).unwrap().len(), 1);
    }

    #[test]
    fn unknown_snapshot_id_lists_nothing() {
        let (db, a1, _b1, a2, _b2) = interleaved_fixture();
        let f = db.upsert_file("x", "x", 1, 0, 0).unwrap();
        db.insert_span(f, a1, a2).unwrap();
        assert!(db.get_files_in_snapshot(9999).unwrap().is_empty());
    }

    // -----------------------------------------------------------------
    // bd DAS-Backup-Manager-cu8 — pruning deleted snapshots
    // -----------------------------------------------------------------

    #[test]
    fn prune_repairs_a_span_whose_first_snap_is_deleted() {
        // The retention-shaped case: the OLDEST snapshot goes. Deleting the whole
        // span would erase the file from every surviving snapshot too.
        let (db, a1, _b1, a2, _b2) = interleaved_fixture();
        let f = db.upsert_file("src/main.rs", "main.rs", 10, 0, 0).unwrap();
        db.insert_span(f, a1, a2).unwrap();

        let stats = db.prune_snapshots(&[a1]).unwrap();
        assert_eq!(stats.snapshots_removed, 1);
        assert_eq!(stats.spans_repaired, 1);
        assert_eq!(stats.spans_removed, 0);
        assert_eq!(stats.files_removed, 0);

        // The file must still be listed in the snapshot that survived.
        let listed = db.get_files_in_snapshot(a2).unwrap();
        assert_eq!(listed.len(), 1, "surviving snapshot lost its file");
        let (first, last): (i64, i64) = db
            .conn
            .query_row("SELECT first_snap, last_snap FROM spans", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((first, last), (a2, a2));
    }

    #[test]
    fn prune_removes_a_span_with_no_survivors_and_its_orphan_file() {
        let (db, a1, _b1, a2, _b2) = interleaved_fixture();
        let f = db.upsert_file("gone.txt", "gone.txt", 1, 0, 0).unwrap();
        db.insert_span(f, a1, a2).unwrap();

        let stats = db.prune_snapshots(&[a1, a2]).unwrap();
        assert_eq!(stats.snapshots_removed, 2);
        assert_eq!(stats.spans_removed, 1);
        assert_eq!(stats.files_removed, 1);
        assert!(db.get_file("gone.txt").unwrap().is_none());
    }

    #[test]
    fn prune_leaves_other_series_untouched() {
        let (db, a1, b1, a2, b2) = interleaved_fixture();
        let proj = db.upsert_file("src/main.rs", "main.rs", 10, 0, 0).unwrap();
        db.insert_span(proj, a1, a2).unwrap();
        let abook = db
            .upsert_file("Library/book.opus", "book.opus", 20, 0, 0)
            .unwrap();
        db.insert_span(abook, b1, b2).unwrap();

        db.prune_snapshots(&[a1, a2]).unwrap();

        assert_eq!(db.get_files_in_snapshot(b1).unwrap().len(), 1);
        assert!(db.get_file("Library/book.opus").unwrap().is_some());
        let remaining: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 2);
    }

    #[test]
    fn prune_repairs_both_ends_of_a_span() {
        // Both endpoints doomed with a survivor between them. Distinguishes
        // `repaired_first + repaired_last` from any other combination of the two —
        // every earlier prune test moves only one end, so `+` vs `-` was invisible.
        let db = Database::open(":memory:").unwrap();
        let a1 = db
            .insert_snapshot("projA", "T1", "projects", "/mnt/t/projects/projA.T1")
            .unwrap();
        let a2 = db
            .insert_snapshot("projA", "T2", "projects", "/mnt/t/projects/projA.T2")
            .unwrap();
        let a3 = db
            .insert_snapshot("projA", "T3", "projects", "/mnt/t/projects/projA.T3")
            .unwrap();
        let f = db.upsert_file("src/main.rs", "main.rs", 10, 0, 0).unwrap();
        db.insert_span(f, a1, a3).unwrap();

        let stats = db.prune_snapshots(&[a1, a3]).unwrap();
        assert_eq!(stats.spans_repaired, 2, "both endpoints should count");
        assert_eq!(stats.spans_removed, 0);
        assert_eq!(stats.snapshots_removed, 2);

        let (first, last): (i64, i64) = db
            .conn
            .query_row("SELECT first_snap, last_snap FROM spans", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((first, last), (a2, a2));
        assert_eq!(db.get_files_in_snapshot(a2).unwrap().len(), 1);
    }

    #[test]
    fn upsert_file_updates_when_only_the_size_changed() {
        // Guards the `||` in upsert_file's change detection: with `&&`, or with
        // either comparison inverted, a size-only change is silently dropped and
        // the index keeps reporting the old size forever.
        let db = Database::open(":memory:").unwrap();
        let id = db.upsert_file("a.txt", "a.txt", 10, 100, 0).unwrap();
        let again = db.upsert_file("a.txt", "a.txt", 999, 100, 0).unwrap();
        assert_eq!(id, again, "same path must reuse the row");
        assert_eq!(db.get_file("a.txt").unwrap().unwrap().size, 999);
    }

    #[test]
    fn upsert_file_updates_when_only_the_mtime_changed() {
        let db = Database::open(":memory:").unwrap();
        db.upsert_file("a.txt", "a.txt", 10, 100, 0).unwrap();
        db.upsert_file("a.txt", "a.txt", 10, 555, 0).unwrap();
        assert_eq!(db.get_file("a.txt").unwrap().unwrap().mtime, 555);
    }

    #[test]
    fn search_passes_prefix_wildcards_through_to_fts() {
        // `repo*` must reach FTS5 unquoted. If the passthrough condition is
        // inverted the term is quoted, `*` becomes a literal, and prefix search
        // silently returns nothing.
        let db = Database::open(":memory:").unwrap();
        let sid = db
            .insert_snapshot("s", "T1", "src", "/mnt/t/src/s.T1")
            .unwrap();
        let f = db
            .upsert_file("report2026.txt", "report2026.txt", 1, 0, 0)
            .unwrap();
        db.insert_span(f, sid, sid).unwrap();
        assert_eq!(db.search("repo*", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_passes_column_filters_through_to_fts() {
        // Exercises the SECOND `||` in the passthrough condition, which the `*`
        // test short-circuits past. `path:deep` is an FTS5 column filter; quoting
        // it would make it a literal phrase and match nothing.
        let db = Database::open(":memory:").unwrap();
        let sid = db
            .insert_snapshot("s", "T1", "src", "/mnt/t/src/s.T1")
            .unwrap();
        let f = db
            .upsert_file("deep/dir/alpha.txt", "alpha.txt", 1, 0, 0)
            .unwrap();
        db.insert_span(f, sid, sid).unwrap();
        assert_eq!(db.search("path:deep", 10).unwrap().len(), 1);
    }

    /// N snapshots in ONE series, so span endpoints can be moved onto each other.
    fn one_series(n: usize) -> (Database, Vec<i64>) {
        let db = Database::open(":memory:").unwrap();
        let ids = (0..n)
            .map(|i| {
                db.insert_snapshot(
                    "projA",
                    &format!("T{i}"),
                    "projects",
                    &format!("/mnt/t/projects/projA.T{i}"),
                )
                .unwrap()
            })
            .collect();
        (db, ids)
    }

    #[test]
    fn prune_merges_a_repaired_span_onto_an_overlapping_one() {
        // The production shape that broke a live run: `.Trash-0` held spans
        // (219,221) AND (220,220) in one series, so pruning 219 moved the first
        // span's start onto 220 and hit UNIQUE(file_id, first_snap) = SQLITE 1555.
        // 7,402,503 such overlapping pairs exist in the production index.
        let (db, ids) = one_series(3);
        let (a1, a2, a3) = (ids[0], ids[1], ids[2]);
        let f = db
            .upsert_file("\u{2e}Trash-0", "\u{2e}Trash-0", 1, 0, 0)
            .unwrap();
        db.insert_span(f, a1, a3).unwrap();
        db.insert_span(f, a2, a2).unwrap();

        let stats = db
            .prune_snapshots(&[a1])
            .expect("overlapping spans must merge, not violate the primary key");
        assert_eq!(stats.snapshots_removed, 1);
        // Two spans in, one out: the merged-away row counts as a removal.
        assert_eq!(stats.spans_removed, 1);
        assert_eq!(stats.spans_repaired, 1);

        // One merged span covering the union, and the file still resolves in both
        // surviving snapshots.
        let rows: Vec<(i64, i64)> = db
            .conn
            .prepare("SELECT first_snap, last_snap FROM spans ORDER BY first_snap")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows, vec![(a2, a3)], "spans should merge to their union");
        assert_eq!(db.get_files_in_snapshot(a2).unwrap().len(), 1);
        assert_eq!(db.get_files_in_snapshot(a3).unwrap().len(), 1);
    }

    #[test]
    fn prune_merges_two_repaired_spans_landing_on_the_same_start() {
        // Repaired-vs-repaired: both spans lose their start and both resolve to
        // the same survivor. Handled by the GROUP BY rather than the upsert.
        let (db, ids) = one_series(4);
        let (a1, a2, a3, a4) = (ids[0], ids[1], ids[2], ids[3]);
        let f = db.upsert_file("x.txt", "x.txt", 1, 0, 0).unwrap();
        db.insert_span(f, a1, a4).unwrap();
        db.insert_span(f, a2, a3).unwrap();

        db.prune_snapshots(&[a1, a2])
            .expect("two repairs onto one start must merge");

        let rows: Vec<(i64, i64)> = db
            .conn
            .prepare("SELECT first_snap, last_snap FROM spans ORDER BY first_snap")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows, vec![(a3, a4)]);
        assert_eq!(db.get_files_in_snapshot(a4).unwrap().len(), 1);
    }

    #[test]
    fn prune_refuses_when_a_span_references_a_missing_snapshot() {
        // bd DAS-Backup-Manager-opd: the production index carries ~15M spans whose
        // endpoints name snapshots that no longer exist. Rewriting such a span
        // re-validates its FKs and aborts the pass minutes in. Fail fast instead,
        // with a diagnosis rather than a raw SqliteFailure.
        let (db, ids) = one_series(3);
        let (a1, a2, a3) = (ids[0], ids[1], ids[2]);
        let f = db.upsert_file("x.txt", "x.txt", 1, 0, 0).unwrap();
        db.insert_span(f, a1, a3).unwrap();

        // Manufacture the corruption the way it exists in production: a snapshot
        // row gone while spans still reference it. Requires bypassing the FK
        // enforcement that would otherwise prevent creating this state.
        db.conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        db.conn
            .execute("DELETE FROM snapshots WHERE id = ?1", [a3])
            .unwrap();
        db.conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let err = db.prune_snapshots(&[a1]).expect_err("must refuse to prune");
        let msg = err.to_string();
        assert!(
            msg.contains("do not exist") && msg.contains("opd"),
            "error should diagnose the cause and name the tracking issue, got: {msg}"
        );

        // And it must refuse WITHOUT having changed anything.
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM spans", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(db.get_snapshot_by_id(a2).is_ok());
    }

    #[test]
    fn prune_of_nothing_changes_nothing() {
        let (db, a1, _b1, a2, _b2) = interleaved_fixture();
        let f = db.upsert_file("x", "x", 1, 0, 0).unwrap();
        db.insert_span(f, a1, a2).unwrap();
        let stats = db.prune_snapshots(&[]).unwrap();
        assert_eq!(stats, crate::reconcile::PruneStats::default());
        assert_eq!(db.get_files_in_snapshot(a1).unwrap().len(), 1);
    }

    #[test]
    fn prune_drops_fts_entries_with_the_file() {
        // files_ad trigger keeps files_fts in step — proves search stops serving
        // a purged path, which is cu8's whole point.
        let (db, a1, _b1, a2, _b2) = interleaved_fixture();
        let f = db.upsert_file("secret.env", "secret.env", 1, 0, 0).unwrap();
        db.insert_span(f, a1, a2).unwrap();
        assert_eq!(db.search("secret.env", 10).unwrap().len(), 1);

        db.prune_snapshots(&[a1, a2]).unwrap();
        assert!(
            db.search("secret.env", 10).unwrap().is_empty(),
            "search still returns a file whose snapshots were pruned"
        );
    }

    #[test]
    fn opens_in_memory() {
        let db = Database::open(":memory:").unwrap();
        assert!(db.conn.is_autocommit());
    }

    #[test]
    fn creates_schema() {
        let db = Database::open(":memory:").unwrap();
        let tables: Vec<String> = db
            .conn
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
        let mode: String = db
            .conn
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
        let fk: i64 = db
            .conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn insert_snapshot() {
        let db = Database::open(":memory:").unwrap();
        let id = db
            .insert_snapshot(
                "root",
                "20260221T0304",
                "nvme",
                "/mnt/backup/nvme/root.20260221T0304",
            )
            .unwrap();
        assert!(id > 0);
    }

    #[test]
    fn get_snapshot_by_path() {
        let db = Database::open(":memory:").unwrap();
        db.insert_snapshot(
            "root",
            "20260221T0304",
            "nvme",
            "/mnt/backup/nvme/root.20260221T0304",
        )
        .unwrap();
        let snap = db
            .get_snapshot("/mnt/backup/nvme/root.20260221T0304")
            .unwrap()
            .unwrap();
        assert_eq!(snap.name, "root");
        assert_eq!(snap.ts, "20260221T0304");
        assert_eq!(snap.source, "nvme");
    }

    #[test]
    fn snapshot_exists() {
        let db = Database::open(":memory:").unwrap();
        db.insert_snapshot(
            "root",
            "20260221T0304",
            "nvme",
            "/mnt/backup/nvme/root.20260221T0304",
        )
        .unwrap();
        assert!(
            db.snapshot_exists("/mnt/backup/nvme/root.20260221T0304")
                .unwrap()
        );
        assert!(!db.snapshot_exists("/mnt/backup/nvme/nonexistent").unwrap());
    }

    #[test]
    fn list_snapshots() {
        let db = Database::open(":memory:").unwrap();
        db.insert_snapshot(
            "root",
            "20260220T0300",
            "nvme",
            "/mnt/backup/nvme/root.20260220T0300",
        )
        .unwrap();
        db.insert_snapshot(
            "root",
            "20260221T0300",
            "nvme",
            "/mnt/backup/nvme/root.20260221T0300",
        )
        .unwrap();
        db.insert_snapshot(
            "home",
            "20260221T0300",
            "nvme",
            "/mnt/backup/nvme/home.20260221T0300",
        )
        .unwrap();
        let snaps = db.list_snapshots().unwrap();
        assert_eq!(snaps.len(), 3);
        assert_eq!(snaps[0].ts, "20260220T0300"); // ordered by ts
    }

    #[test]
    fn upsert_file_new() {
        let db = Database::open(":memory:").unwrap();
        let id = db
            .upsert_file("home/bosco/.zshrc", ".zshrc", 1024, 1708500000, 0)
            .unwrap();
        assert!(id > 0);
    }

    #[test]
    fn get_file_by_path() {
        let db = Database::open(":memory:").unwrap();
        db.upsert_file("home/bosco/.zshrc", ".zshrc", 1024, 1708500000, 0)
            .unwrap();
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
        let snap_id = db
            .insert_snapshot("root", "20260221T0304", "nvme", "/snap1")
            .unwrap();
        let file_id = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        db.insert_span(file_id, snap_id, snap_id).unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM spans WHERE file_id = ?1",
                [file_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn extend_span() {
        let db = Database::open(":memory:").unwrap();
        let s1 = db
            .insert_snapshot("root", "20260220T0300", "nvme", "/snap1")
            .unwrap();
        let s2 = db
            .insert_snapshot("root", "20260221T0300", "nvme", "/snap2")
            .unwrap();
        let fid = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        db.insert_span(fid, s1, s1).unwrap();
        let extended = db.extend_span(fid, s1, s2).unwrap();
        assert!(extended);
        let last: i64 = db
            .conn
            .query_row(
                "SELECT last_snap FROM spans WHERE file_id = ?1 AND first_snap = ?2",
                rusqlite::params![fid, s1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(last, s2);
    }

    #[test]
    fn extend_span_fails_when_no_match() {
        let db = Database::open(":memory:").unwrap();
        let s1 = db
            .insert_snapshot("root", "20260220T0300", "nvme", "/snap1")
            .unwrap();
        let s3 = db
            .insert_snapshot("root", "20260222T0300", "nvme", "/snap3")
            .unwrap();
        let fid = db.upsert_file("a.txt", "a.txt", 100, 1000, 0).unwrap();
        db.insert_span(fid, s1, s1).unwrap();
        let extended = db.extend_span(fid, s3, s3).unwrap();
        assert!(!extended);
    }

    fn setup_search_db() -> Database {
        let db = Database::open(":memory:").unwrap();
        let s1 = db
            .insert_snapshot("root", "20260220T0300", "nvme", "/snap1")
            .unwrap();
        let f1 = db
            .upsert_file("docs/report.pdf", "report.pdf", 1000, 100, 0)
            .unwrap();
        let f2 = db
            .upsert_file("photos/photo.jpg", "photo.jpg", 2000, 200, 0)
            .unwrap();
        let f3 = db
            .upsert_file(
                "docs/annual-report.docx",
                "annual-report.docx",
                3000,
                300,
                0,
            )
            .unwrap();
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

    // -----------------------------------------------------------------
    // backup_runs table tests
    // -----------------------------------------------------------------

    #[test]
    fn insert_and_get_backup_run() {
        let db = Database::open(":memory:").unwrap();
        let ts = 1709000000_i64;
        let id = db
            .insert_backup_run(&NewBackupRun {
                timestamp: ts,
                success: true,
                mode: "incremental",
                snaps_created: 5,
                snaps_sent: 5,
                bytes_sent: 1_073_741_824,
                duration_secs: 3600,
                errors: &[],
            })
            .unwrap();
        assert!(id > 0);

        let history = db.get_backup_history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, id);
        assert_eq!(history[0].timestamp, ts);
        assert!(history[0].success);
        assert_eq!(history[0].mode, "incremental");
        assert_eq!(history[0].snaps_created, 5);
        assert_eq!(history[0].snaps_sent, 5);
        assert_eq!(history[0].bytes_sent, 1_073_741_824);
        assert_eq!(history[0].duration_secs, 3600);
        assert!(history[0].errors.is_empty());
    }

    #[test]
    fn backup_run_with_errors() {
        let db = Database::open(":memory:").unwrap();
        let errors = vec!["btrbk failed".to_string(), "target offline".to_string()];
        let id = db
            .insert_backup_run(&NewBackupRun {
                timestamp: 1709000000,
                success: false,
                mode: "full",
                snaps_created: 2,
                snaps_sent: 0,
                bytes_sent: 0,
                duration_secs: 60,
                errors: &errors,
            })
            .unwrap();

        let history = db.get_backup_history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert!(!history[0].success);
        assert_eq!(history[0].errors.len(), 2);
        assert_eq!(history[0].errors[0], "btrbk failed");
        assert_eq!(history[0].errors[1], "target offline");
        assert_eq!(history[0].id, id);
    }

    #[test]
    fn backup_history_ordered_newest_first() {
        let db = Database::open(":memory:").unwrap();
        db.insert_backup_run(&NewBackupRun {
            timestamp: 1709000000,
            success: true,
            mode: "incremental",
            snaps_created: 1,
            snaps_sent: 1,
            bytes_sent: 100,
            duration_secs: 10,
            errors: &[],
        })
        .unwrap();
        db.insert_backup_run(&NewBackupRun {
            timestamp: 1709100000,
            success: true,
            mode: "full",
            snaps_created: 5,
            snaps_sent: 5,
            bytes_sent: 500,
            duration_secs: 60,
            errors: &[],
        })
        .unwrap();
        let errs = vec!["fail".to_string()];
        db.insert_backup_run(&NewBackupRun {
            timestamp: 1709200000,
            success: false,
            mode: "incremental",
            snaps_created: 0,
            snaps_sent: 0,
            bytes_sent: 0,
            duration_secs: 5,
            errors: &errs,
        })
        .unwrap();

        let history = db.get_backup_history(10).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].timestamp, 1709200000); // newest first
        assert_eq!(history[1].timestamp, 1709100000);
        assert_eq!(history[2].timestamp, 1709000000);
    }

    #[test]
    fn backup_history_respects_limit() {
        let db = Database::open(":memory:").unwrap();
        for i in 0..5 {
            db.insert_backup_run(&NewBackupRun {
                timestamp: 1709000000 + i * 86400,
                success: true,
                mode: "incremental",
                snaps_created: 1,
                snaps_sent: 1,
                bytes_sent: 100,
                duration_secs: 10,
                errors: &[],
            })
            .unwrap();
        }
        let history = db.get_backup_history(3).unwrap();
        assert_eq!(history.len(), 3);
    }

    // -----------------------------------------------------------------
    // target_usage table tests
    // -----------------------------------------------------------------

    #[test]
    fn insert_and_get_target_usage() {
        let db = Database::open(":memory:").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let id = db
            .insert_target_usage(
                now,
                "primary-22tb",
                22_000_000_000_000,
                5_000_000_000_000,
                150,
            )
            .unwrap();
        assert!(id > 0);

        let usage = db.get_target_usage_history("primary-22tb", 30).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].target_label, "primary-22tb");
        assert_eq!(usage[0].total_bytes, 22_000_000_000_000);
        assert_eq!(usage[0].used_bytes, 5_000_000_000_000);
        assert_eq!(usage[0].snapshot_count, 150);
    }

    #[test]
    fn target_usage_filters_by_label() {
        let db = Database::open(":memory:").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        db.insert_target_usage(
            now,
            "primary-22tb",
            22_000_000_000_000,
            5_000_000_000_000,
            150,
        )
        .unwrap();
        db.insert_target_usage(
            now,
            "system-recovery-B-2tb",
            2_000_000_000_000,
            500_000_000_000,
            7,
        )
        .unwrap();

        let primary = db.get_target_usage_history("primary-22tb", 30).unwrap();
        assert_eq!(primary.len(), 1);
        assert_eq!(primary[0].target_label, "primary-22tb");

        let system = db
            .get_target_usage_history("system-recovery-B-2tb", 30)
            .unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0].target_label, "system-recovery-B-2tb");
    }

    #[test]
    fn target_usage_ordered_by_timestamp() {
        let db = Database::open(":memory:").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        db.insert_target_usage(now - 86400, "test", 100, 50, 5)
            .unwrap();
        db.insert_target_usage(now, "test", 100, 60, 6).unwrap();
        db.insert_target_usage(now - 172800, "test", 100, 40, 4)
            .unwrap();

        let usage = db.get_target_usage_history("test", 30).unwrap();
        assert_eq!(usage.len(), 3);
        // Should be ordered oldest first (ASC)
        assert!(usage[0].timestamp < usage[1].timestamp);
        assert!(usage[1].timestamp < usage[2].timestamp);
    }
}
