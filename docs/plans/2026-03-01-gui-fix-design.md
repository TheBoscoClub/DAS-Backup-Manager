# ButteredDASD GUI Fix — Design Document

**Date**: 2026-03-01
**Version**: 0.6.0 → 0.7.0
**Scope**: Fix all broken GUI functionality, complete D-Bus integration, install missing artifacts

## Problem Statement

The ButteredDASD GUI (btrdasd-gui) is non-functional beyond launching. Root causes:

1. **Wrong config path**: Every D-Bus call passes `/etc/btrbk/btrbk.conf` but the helper expects `/etc/das-backup/config.toml` (TOML format). The helper's TOML parser rejects btrbk's native syntax.
2. **No D-Bus methods for index data**: The GUI opens the SQLite database directly as an unprivileged user. The root-owned DB in WAL mode cannot be read by user `bosco`.
3. **IndexRunner spawns CLI as bosco**: The Re-index button runs `btrdasd walk` as the current user instead of using the D-Bus `IndexWalk` method.
4. **BackupPanel parser expects btrbk format**: The source/target parser reads `volume X` / `subvolume X` lines, but `ConfigGet` returns TOML.
5. **HealthDashboard data contract mismatch**: GUI expects `drives`, `growth`, `services` JSON keys; helper returns `targets` with minimal fields.
6. **D-Bus signal type mismatch**: `JobProgress` percent is `y` (uint8) but GUI connects with `int`.
7. **DB column mismatch**: GUI queries `label` but actual column is `target_label`.
8. **Man page not installed**: Source exists at `docs/btrdasd.1`, no CMake install rule, header says v0.5.1.
9. **Shell completions not installed**: `btrdasd completions` works but output never piped to system dirs.

## Design

### 1. Fix config path constant

Replace `/etc/btrbk/btrbk.conf` with `/etc/das-backup/config.toml` in:

- `mainwindow.cpp:438,449` — `updateStatusBar()`
- `configdialog.cpp:27` — constructor
- `backuppanel.cpp:21` — constructor
- `healthdashboard.cpp:62` — constructor

ConfigDialog title: "btrbk Configuration" → "DAS Backup Configuration"

### 2. New D-Bus methods for index data

Add to `btrdasd-helper.rs`:

| Method | D-Bus Signature | Returns |
|--------|----------------|---------|
| `IndexStats(db_path)` | `s → s` | JSON: `{snapshots, files, spans, db_size_bytes}` |
| `IndexListSnapshots(db_path)` | `s → s` | JSON array of snapshot objects |
| `IndexListFiles(db_path, snapshot_id)` | `sx → s` | JSON array of file objects |
| `IndexSearch(db_path, query, limit)` | `ssx → s` | JSON array of search results |

These call `buttered_dasd::db::Database` library functions directly. No polkit required for read-only operations — add `org.dasbackup.index.read` action with `allow_active: yes`.

### 3. Remove direct DB access from GUI

- Delete `gui/src/database.cpp` and `gui/src/database.h`
- Replace all `Database*` usage with `DBusClient` calls:
  - `SnapshotModel::reload()` → `indexListSnapshots()`
  - `FileModel::loadSnapshot()` → `indexListFiles()`
  - `SearchModel::executeSearch()` → `indexSearch()`
  - `MainWindow::showStats()` → `indexStats()`
  - `MainWindow::updateStatusBar()` → `indexStats()`
  - `BackupHistoryView` → backup history via D-Bus
- Remove `Qt6::Sql` from `gui/CMakeLists.txt`
- Remove database tests that used direct SQLite access

### 4. IndexRunner → D-Bus

Replace QProcess-based `IndexRunner` with D-Bus:
- `IndexRunner::run()` → calls `m_client->indexWalk()`
- Progress via existing `JobProgress` D-Bus signal
- Remove QProcess member and binary path resolution

### 5. BackupPanel TOML parser

Replace btrbk-format parser with simple TOML extraction:
- Match `label = "..."` under `[[source]]` and `[[target]]` sections
- Match `name = "..."` under `[[source.subvolumes]]`
- Match `manual_only = true/false`
- Build checkbox UI from extracted data

### 6. Extend HealthQuery response

Add to the Rust helper's `health_query()` return JSON:

```json
{
  "status": "healthy|warning|critical",
  "targets": [{
    "label": "...", "serial": "...", "mounted": bool,
    "total_bytes": N, "used_bytes": N, "usage_percent": F,
    "snapshot_count": N, "smart_status": "PASSED|FAILED",
    "temperature_c": N|null, "power_on_hours": N|null, "errors": N|null
  }],
  "growth": [{
    "label": "...",
    "entries": [{"date": "YYYY-MM-DD", "used_bytes": N}]
  }],
  "services": {
    "btrbk_available": bool,
    "timer_enabled": bool,
    "timer_next": "..." | null,
    "last_backup": "..." | null,
    "drives_mounted": N
  },
  "warnings": ["..."]
}
```

- SMART data: `smartctl --json` per target serial
- Growth: parse `/var/lib/das-backup/growth.log`
- Services: `systemctl is-active das-backup.timer`, timer info

Fix GUI JSON key: `drives` → `targets`.

### 7. Fix remaining bugs

- **D-Bus signal type**: Change Rust helper `JobProgress` percent from `y` (byte) to `i` (int32). Simpler than changing all Qt slots.
- **Man page version**: Update `docs/btrdasd.1` header from `0.5.1` to `0.6.0`.
- **Config version**: `btrdasd setup --upgrade` should update config.toml version field.

### 8. CMake install rules

Add to root `CMakeLists.txt`:
- Man page: `install(FILES docs/btrdasd.1 DESTINATION share/man/man1)`
- Shell completions: generated at install time via `btrdasd completions <shell>`
