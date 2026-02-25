# ButteredDASD — Content Indexer for DAS Backup Snapshots

**Binary**: `btrdasd` | **Version**: 0.5.1 | **Language**: Rust (edition 2024)

## Overview

ButteredDASD is a content indexer that builds a searchable SQLite FTS5 database of every file across all BTRFS snapshots on DAS backup targets. It enables instant full-text search across hundreds of snapshots without mounting or traversing filesystem trees. It also includes an interactive installer for configuring the full DAS + BTRFS backup pipeline.

**Scope**: ButteredDASD indexes BTRFS snapshots on Direct-Attached Storage. NAS, SAN, cloud storage, and non-BTRFS filesystems are permanently out of scope. Suggestions and contributions within this scope are very welcome.

## Architecture

```
backup-run.sh                   btrdasd CLI
     │                              │
     └──── run_indexer() ───────────┤
                                    │
                          ┌─────────┴──────────────────┐
                          │         │                   │
                          ▼         ▼                   ▼
                      walk     search/list/info      setup
                          │         │                   │
                    ┌─────┴─────┐   │        ┌──────────┴──────────┐
                    ▼           ▼   │        ▼         ▼           ▼
              discover      index   │     wizard   templates  installer
              snapshots   snapshot  │        │         │           │
                    │         │     │        ▼         ▼           ▼
                    │    scan │     │     Config   btrbk.conf  write files
                    │    dir  │     │     (.toml)  systemd     manifest
                    │         │     │              cron/script
                    ▼         ▼     ▼
              ┌─────────────────────────────┐
              │   SQLite Database            │
              │   backup-index.db            │
              │   ┌──────────────────────┐  │
              │   │ snapshots             │  │
              │   │ files + files_fts     │  │
              │   │ spans                 │  │
              │   └──────────────────────┘  │
              └─────────────────────────────┘
```

### Modules

| Module | File | Purpose |
|--------|------|---------|
| `db` | `src/db.rs` | SQLite connection, schema, CRUD, FTS5 search |
| `scanner` | `src/scanner.rs` | Filesystem traversal with walkdir |
| `indexer` | `src/indexer.rs` | Snapshot discovery, span logic, walk orchestration |
| `main` | `src/main.rs` | CLI with clap subcommands |
| `setup/mod` | `src/setup/mod.rs` | Setup subcommand routing and root check |
| `setup/config` | `src/setup/config.rs` | TOML config types with serde serialization |
| `setup/detect` | `src/setup/detect.rs` | System detection (devices, subvols, init, packages) |
| `setup/templates` | `src/setup/templates.rs` | Template engine for btrbk.conf, systemd, cron, scripts |
| `setup/installer` | `src/setup/installer.rs` | Install/uninstall/upgrade/check with manifest tracking |
| `setup/wizard` | `src/setup/wizard.rs` | 10-step interactive dialoguer wizard |

## Database Schema

### Tables

**snapshots** — One row per indexed BTRFS snapshot.

| Column | Type | Description |
|--------|------|-------------|
| id | INTEGER PK | Auto-increment ID |
| name | TEXT | Snapshot name (e.g., `root`) |
| ts | TEXT | Timestamp (e.g., `20260221T0304`) |
| source | TEXT | Source directory (e.g., `nvme`) |
| path | TEXT UNIQUE | Full filesystem path to snapshot |
| indexed_at | INTEGER | Unix timestamp when indexed |

**files** — One row per unique file path. Updated when file metadata changes.

| Column | Type | Description |
|--------|------|-------------|
| id | INTEGER PK | Auto-increment ID |
| path | TEXT UNIQUE | Relative path within snapshot |
| name | TEXT | Basename (e.g., `report.pdf`) |
| size | INTEGER | File size in bytes |
| mtime | INTEGER | Last modification time (Unix epoch) |
| type | INTEGER | 0=regular, 1=directory, 2=symlink, 3=other |

**spans** — Tracks which snapshots contain which files. Span-based deduplication means an unchanged file present in snapshots 5 through 12 is stored as a single row `(file_id, first_snap=5, last_snap=12)`.

| Column | Type | Description |
|--------|------|-------------|
| file_id | INTEGER FK | References files(id) |
| first_snap | INTEGER FK | First snapshot containing this file version |
| last_snap | INTEGER FK | Last snapshot containing this file version |

**files_fts** — FTS5 virtual table synced from `files` via triggers. Enables full-text search on file names and paths.

### Span Logic

When a new snapshot is indexed:

1. **Scan** the snapshot directory, collecting all file entries
2. **For each file**: check if it exists in the previous snapshot with the same size and mtime
   - **Unchanged**: Extend the existing span (`last_snap = new_snapshot_id`)
   - **Changed**: Upsert the file record with new metadata, create a new span
   - **New**: Insert a new file record and create a new span

This approach dramatically reduces database size — a file unchanged across 100 snapshots requires 1 file row and 1 span row instead of 100 rows.

### Performance Indexes

| Index | Columns | Purpose |
|-------|---------|---------|
| `idx_snapshots_source_name` | source, name | Group snapshots by source during walk |
| `idx_snapshots_ts` | ts | Order snapshots chronologically |
| `idx_spans_file_id` | file_id | Fast lookup of spans for a file |
| `idx_files_name` | name | Direct file name lookups |
| `idx_files_path` | path (UNIQUE) | File deduplication |
| `idx_spans_last` | last_snap | Span extension queries |

## CLI Usage

### Index a backup target

```bash
btrdasd walk /mnt/backup-hdd
btrdasd walk /mnt/backup-hdd --db /custom/path/backup-index.db
```

Expected directory structure on the backup target:

```
/mnt/backup-hdd/
  nvme/                         # source directory
    root.20260221T0304/         # snapshot (name.timestamp)
    root.20260222T0304/
    home.20260221T0304/
  sata/
    data.20260221T0304/
```

Output:

```
Discovered: 6 snapshots
Indexed:    4 new
Skipped:    2 already indexed
  1523 files (1200 new, 300 extended, 23 changed, 0 errors)
  987 files (800 new, 187 extended, 0 changed, 0 errors)
```

### Search for files

```bash
btrdasd search "report.pdf"
btrdasd search "*.log" --limit 20
btrdasd search "report*"           # FTS5 prefix search
```

Output (tab-separated):

```
path/to/report.pdf  15234  1708534800  nvme/root.20260221  nvme/root.20260225
```

### List files in a snapshot

```bash
btrdasd list nvme/root.20260221T0304
btrdasd list "root.20260221*"       # pattern matching
```

### Show database stats

```bash
btrdasd info
```

Output:

```
Snapshots:  42
Files:      158234
Spans:      23456
DB size:    12845056 bytes
```

### Interactive setup

```bash
sudo btrdasd setup                  # Fresh install (10-step wizard)
sudo btrdasd setup --modify         # Re-open wizard with existing config
sudo btrdasd setup --upgrade        # Regenerate files after binary update
sudo btrdasd setup --uninstall      # Remove all generated files
sudo btrdasd setup --check          # Validate config and dependencies
```

See [INSTALL.md](INSTALL.md) for full installer documentation.

### Default database path

All commands use `--db /var/lib/das-backup/backup-index.db` by default. Override with `--db <path>`.

## Integration with backup-run.sh

The backup script calls `btrdasd` after btrbk creates snapshots:

```zsh
run_indexer() {
    local indexer="${BTRDASD_BIN:-/usr/local/bin/btrdasd}"
    # ... soft-fail if binary missing or indexer errors
}
```

- **Soft-fail**: Indexing errors never abort the backup
- **Environment variable**: Set `BTRDASD_BIN` to override the binary path
- **Email report**: Indexer status appears in the backup status email

## Installation

See [INSTALL.md](INSTALL.md) for comprehensive installation instructions including:
- Quick start with `btrdasd setup` wizard
- Manual installation
- Docker deployment
- CMake build options

## Development

```bash
cd indexer
cargo test              # 62 tests (37 unit + 16 setup + 9 integration)
cargo clippy            # Lint check
cargo fmt --check       # Format check
cargo audit             # Security audit
cargo build --release   # Release build (~6.6MB binary)
```

## Design Decisions

1. **Rust over C++**: Chosen for memory safety in the data-intensive indexing pipeline. The KDE Plasma GUI remains C++/Qt6 since KF6 classes require native C++ integration.

2. **Bundled SQLite**: The `rusqlite` crate uses the `bundled` feature to compile SQLite from source, guaranteeing FTS5 availability regardless of system SQLite configuration.

3. **Span-based storage**: Instead of a naive file-per-snapshot model (which would create millions of rows), spans compress unchanged file presence across consecutive snapshots into single rows.

4. **WAL journal mode**: Enables concurrent reads during writes, important when the GUI reads the database while the indexer is running.

5. **FTS5 with triggers**: The FTS5 virtual table is synced automatically via INSERT/UPDATE/DELETE triggers, ensuring the search index is always consistent.

6. **Config-driven installer**: The TOML configuration is the single source of truth. All generated files (btrbk.conf, systemd units, scripts) are reproducible from the config, enabling clean upgrades and uninstalls via manifest tracking.

7. **Distro-agnostic design**: The installer detects the init system (systemd/sysvinit/OpenRC) and package manager at runtime, generating appropriate service files or cron entries for the host platform.
