# Content Indexer Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a C++20 CLI tool (`das-index`) that indexes file metadata from btrbk snapshots into a SQLite FTS5 database, using span-based storage to efficiently track files across hundreds of snapshots.

**Architecture:** Three-layer design — Database (SQLite with FTS5 and WAL), Scanner (filesystem walker using std::filesystem), and Indexer (orchestrator that discovers new snapshots on a backup target, walks them, and updates the DB with span logic). The CLI dispatches subcommands: `walk`, `search`, `list`, `info`.

**Tech Stack:** C++20, SQLite 3.51 (FTS5, WAL), GTest 1.17, CMake 4.2, std::filesystem

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

## Task 1: CMake build system for indexer

**Files:**
- Create: `indexer/CMakeLists.txt`
- Modify: `CMakeLists.txt` (uncomment `add_subdirectory(indexer)`)
- Create: `indexer/tests/CMakeLists.txt`

**Step 1: Create indexer/CMakeLists.txt**

```cmake
add_executable(das-index
    src/main.cpp
    src/db.cpp
    src/scanner.cpp
    src/indexer.cpp
)

target_include_directories(das-index PRIVATE src)
target_compile_features(das-index PRIVATE cxx_std_20)
target_compile_options(das-index PRIVATE -Wall -Wextra -Wpedantic -Werror)
target_link_libraries(das-index PRIVATE sqlite3)

install(TARGETS das-index DESTINATION bin)

# Tests
enable_testing()
add_subdirectory(tests)
```

**Step 2: Create indexer/tests/CMakeLists.txt**

```cmake
find_package(GTest REQUIRED)

add_executable(indexer-tests
    test_db.cpp
    test_scanner.cpp
    test_indexer.cpp
    ../src/db.cpp
    ../src/scanner.cpp
    ../src/indexer.cpp
)

target_include_directories(indexer-tests PRIVATE ../src)
target_compile_features(indexer-tests PRIVATE cxx_std_20)
target_link_libraries(indexer-tests PRIVATE GTest::gtest_main sqlite3)

include(GoogleTest)
gtest_discover_tests(indexer-tests)
```

**Step 3: Create stub source files**

Create minimal stubs for `indexer/src/main.cpp`, `indexer/src/db.h`, `indexer/src/db.cpp`,
`indexer/src/scanner.h`, `indexer/src/scanner.cpp`, `indexer/src/indexer.h`, `indexer/src/indexer.cpp`,
and stub test files `indexer/tests/test_db.cpp`, `indexer/tests/test_scanner.cpp`,
`indexer/tests/test_indexer.cpp` (each with a single trivial passing test).

**Step 4: Uncomment add_subdirectory in top-level CMakeLists.txt**

```cmake
add_subdirectory(indexer)
```

**Step 5: Build and verify**

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Debug
cmake --build build
ctest --test-dir build --output-on-failure
```

Expected: Build succeeds, all stub tests pass.

**Step 6: Commit**

```bash
git add indexer/ CMakeLists.txt
git commit -m "feat(indexer): scaffold C++ indexer with CMake, GTest, and stub sources"
```

---

## Task 2: Database layer — schema and connection

**Files:**
- Modify: `indexer/src/db.h`
- Modify: `indexer/src/db.cpp`
- Modify: `indexer/tests/test_db.cpp`

**Step 1: Write failing tests for DB initialization**

In `test_db.cpp`:
- `TEST(Database, OpensInMemory)` — construct Database(":memory:"), verify it doesn't throw
- `TEST(Database, CreatesSchema)` — open, verify `snapshots`, `files`, `spans`, `files_fts` tables exist via `SELECT name FROM sqlite_master`
- `TEST(Database, WALMode)` — verify `PRAGMA journal_mode` returns `wal`
- `TEST(Database, PRAGMAOptimizeOnClose)` — verify destructor calls `PRAGMA optimize`

**Step 2: Run tests, verify they fail**

```bash
cmake --build build && ctest --test-dir build -R Database --output-on-failure
```

**Step 3: Implement Database class**

`db.h`: Class with constructor(path), destructor, `sqlite3*` handle, `exec()` helper, `prepare()` helper.

`db.cpp`: Constructor opens DB, sets WAL mode, enables foreign keys, runs schema SQL. Destructor runs `PRAGMA optimize` and closes handle. Use RAII — no manual cleanup.

**Step 4: Run tests, verify pass**

**Step 5: Commit**

```bash
git commit -m "feat(indexer): database layer with schema init, WAL, FTS5"
```

---

## Task 3: Database layer — snapshot CRUD

**Files:**
- Modify: `indexer/src/db.h`
- Modify: `indexer/src/db.cpp`
- Modify: `indexer/tests/test_db.cpp`

**Step 1: Write failing tests**

- `TEST(Database, InsertSnapshot)` — insert a snapshot, verify it returns an ID > 0
- `TEST(Database, GetSnapshotByPath)` — insert, then look up by path, verify all fields match
- `TEST(Database, SnapshotExists)` — insert, verify `snapshot_exists(path)` returns true, non-existent returns false
- `TEST(Database, ListSnapshots)` — insert 3, list all, verify count and ordering by ts

**Step 2: Run, verify fail**

**Step 3: Implement**

Methods on Database:
- `int64_t insert_snapshot(name, ts, source, path)` — prepared INSERT, returns `sqlite3_last_insert_rowid`
- `bool snapshot_exists(path)` — prepared SELECT
- `std::optional<Snapshot> get_snapshot(path)` — prepared SELECT
- `std::vector<Snapshot> list_snapshots()` — prepared SELECT ORDER BY ts

Where `Snapshot` is a simple struct `{int64_t id; std::string name, ts, source, path; int64_t indexed_at;}`.

**Step 4: Run, verify pass**

**Step 5: Commit**

```bash
git commit -m "feat(indexer): snapshot CRUD operations"
```

---

## Task 4: Database layer — file CRUD and span operations

**Files:**
- Modify: `indexer/src/db.h`
- Modify: `indexer/src/db.cpp`
- Modify: `indexer/tests/test_db.cpp`

**Step 1: Write failing tests**

- `TEST(Database, InsertFile)` — insert file, verify ID returned
- `TEST(Database, GetFileByPath)` — insert, look up, verify fields
- `TEST(Database, UpsertFileUnchanged)` — insert, upsert with same size/mtime: same ID, no new row
- `TEST(Database, UpsertFileChanged)` — insert, upsert with different size: same ID, updated fields
- `TEST(Database, InsertSpan)` — insert file + snapshot, create span, verify
- `TEST(Database, ExtendSpan)` — create span ending at snap1, extend to snap2, verify last_snap updated
- `TEST(Database, FindExtendableSpan)` — file with span ending at snap N, find it for extension at snap N+1
- `TEST(Database, SpanNotExtendableWhenGap)` — file with span ending at snap N, snap N+2 exists: no extension (gap)

**Step 2: Run, verify fail**

**Step 3: Implement**

Methods:
- `int64_t upsert_file(path, name, size, mtime, type)` — INSERT OR IGNORE, then SELECT id. If existing row has different size/mtime, UPDATE it. Return id.
- `std::optional<File> get_file(path)` — lookup by path
- `void insert_span(file_id, first_snap, last_snap)` — INSERT span
- `bool extend_span(file_id, prev_snap_id, new_snap_id)` — UPDATE spans SET last_snap=new WHERE file_id=X AND last_snap=prev. Returns true if a row was updated.

Where `File` is `{int64_t id; std::string path, name; int64_t size, mtime; int type;}`.

**Step 4: Run, verify pass**

**Step 5: Commit**

```bash
git commit -m "feat(indexer): file CRUD and span operations"
```

---

## Task 5: Database layer — FTS5 search

**Files:**
- Modify: `indexer/src/db.h`
- Modify: `indexer/src/db.cpp`
- Modify: `indexer/tests/test_db.cpp`

**Step 1: Write failing tests**

- `TEST(Database, FTS5SearchByName)` — insert files "report.pdf", "photo.jpg", "annual-report.docx", search "report" returns 2 matches
- `TEST(Database, FTS5SearchByPath)` — insert files with paths "home/bosco/docs/plan.md", "opt/docs/readme.md", search "bosco" returns 1
- `TEST(Database, FTS5PrefixSearch)` — search "rep*" matches "report.pdf" and "annual-report.docx"
- `TEST(Database, FTS5NoResults)` — search "nonexistent" returns empty vector
- `TEST(Database, SearchWithSnapshotInfo)` — insert files with spans, search returns results with snapshot range (first_snap name..last_snap name)

**Step 2: Run, verify fail**

**Step 3: Implement**

Method: `std::vector<SearchResult> search(query, limit=50)` — runs FTS5 MATCH query joined with spans and snapshots to return `{path, name, size, mtime, first_snap_name, last_snap_name, first_snap_ts, last_snap_ts}`.

```sql
SELECT f.path, f.name, f.size, f.mtime,
       s1.name || '.' || s1.ts AS first_snap,
       s2.name || '.' || s2.ts AS last_snap
FROM files_fts
JOIN files f ON f.id = files_fts.rowid
JOIN spans sp ON sp.file_id = f.id
JOIN snapshots s1 ON s1.id = sp.first_snap
JOIN snapshots s2 ON s2.id = sp.last_snap
WHERE files_fts MATCH ?
ORDER BY rank
LIMIT ?
```

**Step 4: Run, verify pass**

**Step 5: Commit**

```bash
git commit -m "feat(indexer): FTS5 search with snapshot range info"
```

---

## Task 6: Scanner — filesystem walker

**Files:**
- Modify: `indexer/src/scanner.h`
- Modify: `indexer/src/scanner.cpp`
- Modify: `indexer/tests/test_scanner.cpp`

**Step 1: Write failing tests**

Use `std::filesystem::temp_directory_path()` to create temporary directory structures.

- `TEST(Scanner, WalksEmptyDirectory)` — empty dir, 0 entries
- `TEST(Scanner, WalksFilesAndDirs)` — create 3 files + 1 subdir with 2 files, returns 6 entries (3 + 1 dir + 2)
- `TEST(Scanner, CapturesMetadata)` — create file with known content, entry has correct size, name, relative path
- `TEST(Scanner, IdentifiesSymlinks)` — create symlink, entry type = 2
- `TEST(Scanner, HandlesPermissionErrors)` — create unreadable dir, scanner soft-fails, returns partial results + error count

**Step 2: Run, verify fail**

**Step 3: Implement**

`struct FileEntry { std::string path; std::string name; int64_t size; int64_t mtime; int type; };`

`struct ScanResult { std::vector<FileEntry> entries; int errors; };`

`ScanResult scan_directory(const std::filesystem::path& root)` — uses `std::filesystem::recursive_directory_iterator` with `directory_options::skip_permission_denied`. Strips root prefix from paths (so paths are relative within snapshot). Catches and counts per-entry errors without aborting.

**Step 4: Run, verify pass**

**Step 5: Commit**

```bash
git commit -m "feat(indexer): filesystem scanner with soft-fail error handling"
```

---

## Task 7: Indexer — snapshot discovery

**Files:**
- Modify: `indexer/src/indexer.h`
- Modify: `indexer/src/indexer.cpp`
- Modify: `indexer/tests/test_indexer.cpp`

**Step 1: Write failing tests**

- `TEST(Indexer, DiscoversSnapshots)` — create temp dirs matching btrbk naming (`nvme/root.20260220T0300`, `nvme/root.20260221T0300`), discovers 2 snapshots with correct name, ts, source
- `TEST(Indexer, ParsesSnapshotName)` — `"root.20260221T0304"` parses to name="root", ts="20260221T0304"
- `TEST(Indexer, SkipsAlreadyIndexed)` — mark one snapshot as indexed in DB, discover returns only the new one
- `TEST(Indexer, DiscoversSources)` — create `nvme/`, `ssd/`, `projects/` dirs with snapshots, source field matches parent dir name

**Step 2: Run, verify fail**

**Step 3: Implement**

```cpp
struct DiscoveredSnapshot {
    std::string name;       // "root"
    std::string ts;         // "20260221T0304"
    std::string source;     // "nvme"
    std::filesystem::path path;  // "/mnt/backup-22tb/nvme/root.20260221T0304"
};

// Walks the backup target, finds snapshot directories matching <name>.<timestamp>
std::vector<DiscoveredSnapshot> discover_snapshots(
    const std::filesystem::path& target_root,
    Database& db
);
```

Logic: iterate first-level subdirs of target_root (these are sources: nvme, ssd, projects, ...). Within each source dir, list entries matching regex `^(.+)\.(\d{8}T\d{4,6})$`. Skip any whose full path is already in `snapshots` table.

**Step 4: Run, verify pass**

**Step 5: Commit**

```bash
git commit -m "feat(indexer): snapshot discovery from backup target"
```

---

## Task 8: Indexer — index a snapshot (span logic)

**Files:**
- Modify: `indexer/src/indexer.h`
- Modify: `indexer/src/indexer.cpp`
- Modify: `indexer/tests/test_indexer.cpp`

**Step 1: Write failing tests**

- `TEST(Indexer, IndexFirstSnapshot)` — create temp snapshot with 3 files, index, DB has 3 files, 3 spans, 1 snapshot
- `TEST(Indexer, IndexSecondSnapshotExtendsSpans)` — index snapshot1 with files A,B,C then create snapshot2 with same files (same size/mtime), index, spans extended (last_snap=snap2), still 3 span rows
- `TEST(Indexer, IndexDetectsNewFiles)` — snapshot1: A,B then snapshot2: A,B,C, index both, 3 files, file C has span starting at snap2
- `TEST(Indexer, IndexDetectsDeletedFiles)` — snapshot1: A,B,C then snapshot2: A,B, index both, file C span ends at snap1
- `TEST(Indexer, IndexDetectsChangedFiles)` — snapshot1: A(100 bytes) then snapshot2: A(200 bytes), index both, A has 2 spans (one for each version), file size updated to 200
- `TEST(Indexer, IndexLargeDirectory)` — create 10,000 files, index completes in less than 5 seconds (performance sanity check)

**Step 2: Run, verify fail**

**Step 3: Implement**

```cpp
struct IndexResult {
    int64_t snapshot_id;
    int files_total;
    int files_new;
    int files_extended;
    int files_changed;
    int scan_errors;
};

IndexResult index_snapshot(
    Database& db,
    const DiscoveredSnapshot& snap,
    int64_t prev_snap_id  // 0 if first snapshot for this source+name
);
```

Algorithm:
1. Scan the snapshot directory via ScanResult
2. Begin transaction
3. Insert snapshot record to get snap_id
4. For each FileEntry:
   a. `upsert_file(path, name, size, mtime, type)` to get file_id
   b. If `prev_snap_id > 0` AND file size+mtime unchanged: `extend_span(file_id, prev_snap_id, snap_id)`
   c. If extension failed (file is new or changed): `insert_span(file_id, snap_id, snap_id)`
5. Commit transaction
6. Return stats

**Performance:** Use a single transaction for the entire snapshot. Pre-fetch the previous snapshot's file set into a `std::unordered_map<std::string, File>` for O(1) comparisons.

**Step 4: Run, verify pass**

**Step 5: Commit**

```bash
git commit -m "feat(indexer): index snapshot with span-based deduplication"
```

---

## Task 9: Indexer — walk command (orchestrate full indexing run)

**Files:**
- Modify: `indexer/src/indexer.h`
- Modify: `indexer/src/indexer.cpp`
- Modify: `indexer/tests/test_indexer.cpp`

**Step 1: Write failing test**

- `TEST(Indexer, WalkIndexesAllNewSnapshots)` — create 2 sources with 2 snapshots each, walk, all 4 indexed, stats printed
- `TEST(Indexer, WalkSkipsAlreadyIndexed)` — index 2, add 1 more, walk, only 1 new indexed
- `TEST(Indexer, WalkOrdersByTimestamp)` — snapshots indexed in chronological order (required for span extension to work)

**Step 2: Run, verify fail**

**Step 3: Implement**

```cpp
struct WalkResult {
    int snapshots_discovered;
    int snapshots_indexed;
    int snapshots_skipped;
    std::vector<IndexResult> results;
};

WalkResult walk(const std::filesystem::path& target_root, Database& db);
```

Logic:
1. `discover_snapshots(target_root, db)` then sort by (source, name, ts)
2. Group by (source, name) so we can identify previous snapshot for span extension
3. For each new snapshot in chronological order:
   - Find its predecessor (latest already-indexed snapshot with same source+name)
   - `index_snapshot(db, snap, prev_snap_id)`
4. Return aggregate stats

**Step 4: Run, verify pass**

**Step 5: Commit**

```bash
git commit -m "feat(indexer): walk command orchestrates full indexing run"
```

---

## Task 10: CLI — main.cpp with subcommands

**Files:**
- Modify: `indexer/src/main.cpp`

**Step 1: Implement CLI**

Subcommands:
- `das-index walk <target-path> [--db <path>]` — index all new snapshots on target
- `das-index search <query> [--db <path>] [--limit N]` — FTS5 search
- `das-index list <snapshot-name> [--db <path>]` — list files in a snapshot
- `das-index info [--db <path>]` — DB stats (snapshot count, file count, span count, DB size)

Default `--db` path: `/var/lib/das-backup/backup-index.db`

No external CLI library needed — simple `argv` parsing with `std::string_view` comparisons.

Output format:
- `walk`: prints per-snapshot stats as it goes, then summary
- `search`: tab-separated: `path  size  mtime  first_snap  last_snap`
- `list`: one path per line
- `info`: key-value pairs

**Step 2: Build and test manually**

```bash
cmake --build build
./build/indexer/das-index info --db :memory:
./build/indexer/das-index search "test" --db :memory:
```

**Step 3: Commit**

```bash
git commit -m "feat(indexer): CLI with walk, search, list, info subcommands"
```

---

## Task 11: Integration with backup-run.sh

**Files:**
- Modify: `scripts/backup-run.sh`

**Step 1: Add indexing call after btrbk completes**

Add a new function `run_indexer()` to backup-run.sh, called after `run_btrbk()` and `capture_usage "after"`:

```zsh
run_indexer() {
    local indexer="/hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/build/indexer/das-index"
    local db="/var/lib/das-backup/backup-index.db"

    if [[ ! -x "$indexer" ]]; then
        log_warn "Content indexer not built -- skipping (build with: cmake --build build)"
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

**Step 2: Test manually** (with DAS mounted)

```bash
sudo ./scripts/backup-run.sh --dryrun  # verify indexer skip message
```

**Step 3: Commit**

```bash
git commit -m "feat: integrate content indexer into backup-run.sh"
```

---

## Task 12: Final verification and release

**Step 1: Run full test suite**

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Debug
cmake --build build
ctest --test-dir build --output-on-failure
```

**Step 2: Build release**

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
```

**Step 3: Update CHANGELOG.md and README.md**

Bump version to 0.3.0, document indexer feature.

**Step 4: Commit and push**

```bash
git add -A
git commit -m "feat(indexer): complete content indexer with FTS5 search and span storage"
git push origin main
```
