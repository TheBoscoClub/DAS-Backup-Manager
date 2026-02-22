# DAS-Backup-Manager — Architecture

**Version**: 0.4.0

This document describes the system architecture, data flows, design decisions, and security posture of the DAS-Backup-Manager project.

## Component Overview

```
┌──────────────────────────────────────────────────────────────────────┐
│                        DAS-Backup-Manager                           │
│                                                                     │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────┐  ┌───────────┐ │
│  │   Scripts    │  │   Indexer    │  │    GUI     │  │ Installer │ │
│  │   (zsh)     │  │   (Rust)    │  │ (C++/Qt6)  │  │  (Rust)   │ │
│  ├─────────────┤  ├──────────────┤  ├────────────┤  ├───────────┤ │
│  │backup-run   │  │ btrdasd CLI  │  │btrdasd-gui │  │btrdasd    │ │
│  │backup-verify│  │  walk        │  │ MainWindow │  │ setup     │ │
│  │boot-archive │  │  search      │  │ Timeline   │  │ --modify  │ │
│  │das-partition│  │  list        │  │ Search     │  │ --upgrade │ │
│  │             │  │  info        │  │ Restore    │  │ --uninstall│
│  │             │  │  setup       │  │ Settings   │  │ --check   │ │
│  └──────┬──────┘  └──────┬───────┘  └─────┬──────┘  └─────┬─────┘ │
│         │                │                 │               │       │
│         ▼                ▼                 ▼               ▼       │
│  ┌──────────┐   ┌────────────────┐  ┌──────────┐  ┌────────────┐  │
│  │  btrbk   │   │   SQLite DB    │  │ SQLite DB│  │   TOML     │  │
│  │          │   │ (read/write)   │  │(read-only)│ │  config    │  │
│  └──────────┘   └────────────────┘  └──────────┘  └────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
```

The system has four major components:

| Component | Language | Binary | Purpose |
|-----------|----------|--------|---------|
| Backup scripts | zsh | N/A | btrbk orchestration, verification, boot archival |
| Content indexer | Rust 2024 | `btrdasd` | SQLite FTS5 database of all files across snapshots |
| KDE Plasma GUI | C++20 | `btrdasd-gui` | Visual browsing, searching, and restoring files |
| Interactive installer | Rust 2024 | `btrdasd setup` | Config-driven setup with template generation |

## Data Flow

### Backup Pipeline

```
1. systemd timer fires (das-backup.timer)
         │
         ▼
2. backup-run.sh (orchestrator)
         │
         ├──▶ btrbk run          → creates BTRFS snapshots on backup targets
         ├──▶ btrbk run (full)   → weekly full backup with send/receive
         ├──▶ rsync              → ESP synchronization to recovery drives
         ├──▶ btrdasd walk       → indexes new snapshots into SQLite
         └──▶ mailx              → sends email report
```

### Indexing Pipeline

```
btrdasd walk /mnt/backup-target
         │
         ├──▶ discover_snapshots()       Scan target for source/name.timestamp dirs
         │         │                     Parse dirname with regex: ^(.+)\.(\d{8}T\d{4,6})$
         │         ▼
         │    Filter out already-indexed snapshots (by path in DB)
         │
         ├──▶ For each new snapshot:
         │         │
         │         ├──▶ scan_directory()   walkdir recursive traversal (soft-fail on errors)
         │         │         │
         │         │         ▼
         │         │    Vec<FileEntry> { path, name, size, mtime, file_type }
         │         │
         │         └──▶ index_snapshot()   Span-based deduplication:
         │                   │              - Unchanged file → extend span (last_snap = new)
         │                   │              - Changed file   → upsert file, new span
         │                   │              - New file       → insert file + span
         │                   ▼
         │              SQLite DB updated
         │
         └──▶ Print summary (discovered/indexed/skipped + per-snapshot stats)
```

### GUI Read Path

```
btrdasd-gui --db /var/lib/das-backup/backup-index.db
         │
         ├──▶ Database::open()            Read-only QSqlDatabase (WAL mode)
         │         │
         │         ├──▶ listSnapshots()   → SnapshotModel (tree: date groups → snapshots)
         │         ├──▶ filesInSnapshot() → FileModel (table: path, size, mtime, type)
         │         └──▶ search()          → SearchModel (table: FTS5 results with span info)
         │
         ├──▶ SnapshotTimeline            Custom QPainter widget (visual timeline)
         ├──▶ SnapshotWatcher             QFileSystemWatcher → auto-detect new snapshots
         ├──▶ IndexRunner                 QProcess wrapper → trigger btrdasd walk
         └──▶ RestoreAction               KIO::copy file restore with destination chooser
```

## Database Architecture

### Schema

The SQLite database at `/var/lib/das-backup/backup-index.db` uses three core tables plus an FTS5 virtual table:

```sql
snapshots (id PK, name, ts, source, path UNIQUE, indexed_at)
files     (id PK, path, name, size, mtime, type)
spans     (file_id FK, first_snap FK, last_snap FK, PK(file_id, first_snap))
files_fts (FTS5 virtual: name, path — synced via triggers)
```

### Span-Based Deduplication

Instead of recording every file in every snapshot (which would produce millions of rows), spans compress consecutive identical file appearances:

```
File: /home/user/document.pdf (unchanged across snapshots 5–12)

Naive model:  8 rows in a join table (1 per snapshot)
Span model:   1 row → spans(file_id=42, first_snap=5, last_snap=12)
```

When snapshot 13 arrives:
- If the file is unchanged → `UPDATE spans SET last_snap=13` (extend)
- If the file changed → new span `(file_id=42, first_snap=13, last_snap=13)` + update file metadata
- If the file is gone → no action (span accurately records last appearance)

### FTS5 Synchronization

Three triggers keep the FTS5 index consistent:

| Trigger | Event | Action |
|---------|-------|--------|
| `files_ai` | `AFTER INSERT ON files` | Insert into FTS5 |
| `files_ad` | `AFTER DELETE ON files` | Delete from FTS5 |
| `files_au` | `AFTER UPDATE ON files` | Delete old + insert new in FTS5 |

### Performance Indexes

| Index | Columns | Query Pattern |
|-------|---------|---------------|
| `idx_snapshots_source_name` | (source, name) | Group snapshots by source during walk |
| `idx_snapshots_ts` | (ts) | Chronological ordering |
| `idx_spans_file_id` | (file_id) | Lookup spans for a file |
| `idx_files_name` | (name) | Direct file name lookups |
| `idx_files_path` | (path) UNIQUE | File deduplication |
| `idx_spans_last` | (last_snap) | Span extension queries |

### Concurrent Access

- **WAL journal mode** enables simultaneous reads and writes
- The indexer (`btrdasd walk`) holds a write connection
- The GUI (`btrdasd-gui`) opens a **read-only** connection
- Both can operate concurrently without locking conflicts
- `PRAGMA optimize` runs on connection close (via Rust `Drop` / C++ destructor)

## Installer Architecture

### Config-Driven Design

The installer uses a TOML configuration file (`/etc/das-backup/config.toml`) as the single source of truth:

```
wizard → Config struct → config.toml (save)
                              │
                              ▼
                    templates::generate() → GeneratedFiles
                              │
                              ▼
                    installer::install() → write files + manifest
```

### Config Sections

| Section | Fields | Purpose |
|---------|--------|---------|
| `general` | version, install_prefix, db_path | Global settings |
| `init` | system (systemd/sysvinit/openrc) | Init system selection |
| `schedule` | incremental, full, randomized_delay_min | Backup timing |
| `source[]` | label, subvolume, mount_point, subvolumes[] | BTRFS sources |
| `target[]` | label, device, mount_point, role, retention | Backup targets |
| `esp` | enabled, source_path, method, packages[] | ESP/boot mirroring |
| `email` | enabled, smtp_host, smtp_port, from, to, tls | Email reports |
| `gui` | install (bool) | GUI installation toggle |

### Template Engine

Templates are rendered programmatically (no external template files):

| Function | Output | Description |
|----------|--------|-------------|
| `render_btrbk_conf()` | `btrbk.conf` | Per-source volume blocks with target retention |
| `render_systemd_service()` | `.service` unit | ExecStart with full flag support |
| `render_systemd_timer()` | `.timer` unit | OnCalendar with RandomizedDelaySec |
| `render_cron_entry()` | cron lines | For sysvinit/OpenRC systems |
| `render_backup_run()` | `backup-run-generated.sh` | Script with DAS serials, mount vars |
| `render_email_conf()` | `das-backup-email.conf` | SMTP configuration |
| `render_esp_hook()` | pacman/apt/dnf hook | Package manager hook for ESP sync |

### System Detection

The `detect` module auto-discovers the host environment:

| Detection | Method | Output |
|-----------|--------|--------|
| Block devices | `lsblk --json` parsing | USB/SATA/NVMe devices with size, serial, partitions |
| BTRFS subvolumes | `btrfs subvolume list` parsing | Subvolume paths and IDs |
| Init system | Binary existence checks (`/sbin/init`, `/sbin/openrc-init`) | systemd/sysvinit/openrc |
| Package manager | Binary existence checks | pacman/apt/dnf/zypper |
| Dependencies | `which` checks for btrbk, btrfs, smartctl, etc. | Missing dependency report |

### Manifest Tracking

Every install writes `/etc/das-backup/.manifest` — a plain-text list of generated file paths. This enables:
- `--uninstall`: Remove exactly the files that were installed
- `--upgrade`: Regenerate the same files from updated config
- `--check`: Verify all manifest files exist and match

## Build System

### CMake + ExternalProject

The root `CMakeLists.txt` orchestrates both C++ and Rust builds:

```cmake
option(BUILD_GUI     "Build KDE Plasma GUI (requires Qt6/KF6)" ON)
option(BUILD_INDEXER "Build btrdasd Rust binary via cargo"      ON)
```

- `BUILD_INDEXER=ON`: Uses `ExternalProject_Add` to invoke `cargo build --release`
- `BUILD_GUI=ON`: Uses `add_subdirectory(gui)` with standard KDE/Qt6 CMake modules
- `BUILD_GUI=OFF`: Skips Qt6/KF6 dependency entirely (headless CLI-only build)

### Docker

The `Dockerfile` provides a headless CLI build:
- **Builder stage**: `rust:1.93-bookworm` — compiles `btrdasd` release binary
- **Runtime stage**: `debian:bookworm-slim` — minimal with `btrfs-progs` and `smartmontools`
- Entrypoint: `btrdasd` (all subcommands available)

## Security & Design Decisions

### Memory Safety

**Rust for the indexer**: The content indexer processes untrusted filesystem data (file paths, names, sizes from backup snapshots). Rust eliminates buffer overflows, use-after-free, and data races at compile time. The only `unsafe` code is a single `libc::geteuid()` call for root detection in the setup module.

**C++20 RAII for the GUI**: The GUI uses Qt6/KF6 which requires C++. All GUI components use:
- Smart pointers (`QScopedPointer`, `std::unique_ptr`) — no raw `new`/`delete` leaks
- Qt parent-child ownership model for widget lifetime
- RAII database connections with UUID-based connection names
- Compiled with `-Wall -Wextra -Wpedantic -Werror` — zero warnings policy

**zsh scripts**: All scripts use `set -euo pipefail` equivalent (`setopt ERR_EXIT NO_UNSET PIPE_FAIL`) for fail-fast behavior.

### SQL Injection Prevention

Every database operation uses parameterized prepared statements exclusively:

```rust
// Rust (rusqlite) — parameterized query
conn.query_row("SELECT id FROM snapshots WHERE path = ?1", params![path], |row| row.get(0))
```

```cpp
// C++ (QSqlQuery) — bound parameters
QSqlQuery q(m_db);
q.prepare("SELECT path, name, size, mtime FROM files WHERE id IN (SELECT file_id FROM spans WHERE first_snap <= ? AND last_snap >= ?)");
q.addBindValue(snapshotId);
q.addBindValue(snapshotId);
```

No string concatenation is used to build SQL queries anywhere in the codebase.

### File Permission Hardening

| File | Mode | Owner | Reason |
|------|------|-------|--------|
| `/etc/das-backup-email.conf` | `0o600` | root | Contains SMTP credentials |
| `/etc/das-backup/config.toml` | `0o644` | root | No secrets (email config separate) |
| Generated scripts | `0o755` | root | Executable by system |
| `/var/lib/das-backup/backup-index.db` | `0o644` | root | Readable by GUI, writable by indexer |

### Database Integrity

- **WAL journal mode**: Prevents corruption from concurrent access, provides atomic commits
- **Foreign keys enabled**: `PRAGMA foreign_keys = ON` enforces referential integrity between spans/files/snapshots
- **PRAGMA optimize on close**: Both Rust (`Drop` impl) and C++ (`~Database`) run `PRAGMA optimize` to maintain query planner statistics
- **Unique constraints**: File paths and snapshot paths have unique indexes preventing duplicates

### Input Validation

- **Snapshot names**: Validated with regex `^(.+)\.(\d{8}T\d{4,6})$` — rejects malformed directories
- **TOML configuration**: Deserialized via `serde` with strongly-typed structs — invalid config fails at parse time
- **FTS5 queries**: The GUI auto-quotes search terms with `"` delimiters, preventing FTS5 syntax injection
- **File paths**: All path operations use `std::path::PathBuf` (Rust) or `QString` (C++) — no raw C string manipulation

### Efficiency

- **Span-based deduplication**: A file unchanged across 100 snapshots = 1 file row + 1 span row (not 100 rows)
- **Incremental indexing**: `btrdasd walk` skips already-indexed snapshots (checked by path in DB)
- **Performance indexes**: 6 targeted indexes for common query patterns (see Database Architecture)
- **Bundled SQLite**: Compiled from source with FTS5 enabled — no dependency on system SQLite version

### Stability

- **Soft-fail indexing**: In `backup-run.sh`, indexing errors are logged but never abort the backup. The backup pipeline always completes regardless of indexer state.
- **Error propagation**: Rust `?` operator propagates errors cleanly up the call chain with descriptive error types
- **Graceful degradation**: The GUI opens the database read-only. If the DB doesn't exist or is locked, the GUI still launches with an empty state.

### Privacy

- **Metadata only**: The database stores file paths, names, sizes, and timestamps — never file contents. No user data is read or stored beyond filesystem metadata.
- **Email config isolation**: SMTP credentials are in a separate file (`/etc/das-backup-email.conf`, mode 0600), not in the main TOML config.
- **No telemetry**: No network connections, analytics, or usage tracking of any kind.

### Database Encryption Assessment

The backup index database stores file paths (e.g., `/home/user/Documents/tax-return-2025.pdf`), names, sizes, and timestamps. While no file contents are stored, file paths can reveal personal information about what files exist on a system.

**Current protections**:
- Filesystem permissions: root-owned, mode 0644
- Database lives at `/var/lib/das-backup/backup-index.db` — local filesystem only
- Not network-exposed in any configuration

**For high-sensitivity deployments**, SQLCipher provides full-database AES-256-CBC encryption:

```toml
# In indexer/Cargo.toml, replace:
rusqlite = { version = "0.38", features = ["bundled"] }
# With:
rusqlite = { version = "0.38", features = ["bundled-sqlcipher"] }
```

This requires a passphrase on every database open (both indexer and GUI), adds approximately 30% query overhead, and increases the binary size.

**Recommendation**: Not enabled by default. Most backup systems (btrbk, rsnapshot, restic, borgbackup) do not encrypt their metadata indexes. The database is local-only and root-readable. For users with high-sensitivity requirements (e.g., shared systems, compliance mandates), the SQLCipher path is documented and straightforward to enable.

## Module Reference

### Rust Indexer (`indexer/src/`)

| Module | File | Lines | Purpose |
|--------|------|-------|---------|
| `db` | `src/db.rs` | ~400 | Database connection, schema, CRUD, FTS5 search, stats |
| `scanner` | `src/scanner.rs` | ~100 | walkdir-based filesystem traversal |
| `indexer` | `src/indexer.rs` | ~250 | Snapshot discovery, span logic, walk orchestration |
| `main` | `src/main.rs` | ~115 | CLI entry point with clap subcommands |
| `setup/mod` | `src/setup/mod.rs` | ~65 | Setup subcommand routing and root check |
| `setup/config` | `src/setup/config.rs` | ~250 | TOML config types with serde |
| `setup/detect` | `src/setup/detect.rs` | ~500 | System detection (devices, init, packages) |
| `setup/templates` | `src/setup/templates.rs` | ~400 | Render btrbk.conf, systemd, cron, scripts |
| `setup/installer` | `src/setup/installer.rs` | ~270 | Install/uninstall/upgrade/check with manifest |
| `setup/wizard` | `src/setup/wizard.rs` | ~985 | 10-step interactive dialoguer wizard |

### KDE Plasma GUI (`gui/src/`)

| Component | Files | Purpose |
|-----------|-------|---------|
| MainWindow | `mainwindow.h/cpp` | KXmlGuiWindow with splitters, toolbar, status bar |
| Database | `database.h/cpp` | Read-only QSqlDatabase wrapper with UUID connections |
| SnapshotModel | `snapshotmodel.h/cpp` | QAbstractItemModel tree (date groups → snapshots) |
| FileModel | `filemodel.h/cpp` | QAbstractTableModel for file listings |
| SearchModel | `searchmodel.h/cpp` | QAbstractTableModel for FTS5 search results |
| SnapshotTimeline | `snapshottimeline.h/cpp` | Custom QPainter widget for visual navigation |
| IndexRunner | `indexrunner.h/cpp` | QProcess wrapper for btrdasd walk |
| SnapshotWatcher | `snapshotwatcher.h/cpp` | QFileSystemWatcher with 30s debounce |
| RestoreAction | `restoreaction.h/cpp` | KIO::copy with file dialog destination |
| SettingsDialog | `settingsdialog.h/cpp` | KConfigDialog for paths and preferences |

### Tests

| Suite | Count | Framework |
|-------|-------|-----------|
| Rust unit tests | 37 | `#[cfg(test)]` modules in lib crate |
| Rust setup tests | 16 | `#[cfg(test)]` modules in setup modules |
| Rust integration tests | 9 | `indexer/tests/integration_test.rs` |
| C++ GUI tests | 4 suites | QTest via ECMAddTests |
| **Total** | **62 Rust + 4 Qt** | |
