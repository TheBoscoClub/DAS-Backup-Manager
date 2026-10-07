# ButteredDASD — Content Indexer for DAS Backup Snapshots

**Binary**: `btrdasd` | **Version**: 0.7.23.0 | **Language**: Rust (edition 2024)

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
| `adopt` | `src/adopt.rs` | Subvolume sync: adopts new subvolumes, retires and revives entries, replaces `config.toml` and `btrbk.conf` together (`btrdasd subvol sync`) |
| `backup` | `src/backup.rs` | btrbk snapshot/send orchestration with volume deduplication; `run_backup_job`, the single entry point shared by `btrdasd backup run` and the GUI (syncs subvolumes first) |
| `btrbk_conf` | `src/btrbk_conf.rs` | `btrbk.conf` renderer, shared by `setup`, the `subvol` commands and sync |
| `caldate` | `src/caldate.rs` | Whole-day `YYYY-MM-DD` calendar arithmetic for adoption, retirement and expiry |
| `config` | `src/config.rs` | TOML config types and validation, DAS/source/target models |
| `db` | `src/db.rs` | SQLite connection, schema, CRUD, FTS5 search |
| `doctor` | `src/doctor.rs` | Subvolume drift detector (`btrdasd doctor --check-drift`) |
| `expire` | `src/expire.rs` | Expiry of retired subvolumes' backups per target and location (`btrdasd subvol expire`) |
| `fsutil` | `src/fsutil.rs` | Atomic file replacement and the `CommandRunner` seam for host commands |
| `health` | `src/health.rs` | Drive health (SMART), mountpoint checks, serial to device resolution, scrub health |
| `mount` | `src/mount.rs` | Auto-mount/unmount of targets and sources with RAII `MountGuard`; finds a target by `mount_uuid` or serial, retries a busy unmount, fails the operation if a target stays mounted |
| `progress` | `src/progress.rs` | `ProgressCallback` trait and log levels shared by the CLI and the D-Bus helper (the helper turns events into ordered signals) |
| `recovery_os` | `src/recovery_os.rs` | Read-only inspection of the independent OS on each mirror target: whether it has fallen behind the host, and whether booting it would run btrbk (what its systemd trees and cron start, what those run, and the btrbk config each would use) (`btrdasd recovery-os`) |
| `report` | `src/report.rs` | Backup report formatting |
| `restore` | `src/restore.rs` | File and snapshot restore via btrfs send/receive, gated by `[restore] allowed_roots` and a denylist |
| `schedule` | `src/schedule.rs` | systemd timer management (show/set/enable/disable) |
| `scrub` | `src/scrub.rs` | Scheduled BTRFS scrub engine for the DAS filesystems |
| `subvol` | `src/subvol.rs` | Subvolume CRUD operations (`btrdasd subvol add/remove/set-manual/set-auto`) |
| `scanner` | `src/scanner.rs` | Filesystem traversal with walkdir |
| `indexer` | `src/indexer.rs` | Snapshot discovery, span logic, walk orchestration |
| `reconcile` | `src/reconcile.rs` | Prune index rows for snapshots gone from disk; mountpoint-gated |
| `forget` | `src/forget.rs` | Snapshot selection + deletion for `forget`/`purge`; live-series guard |
| `main` | `src/main.rs` | CLI with clap subcommands |
| `setup/mod` | `src/setup/mod.rs` | Setup subcommand routing and root check |
| `setup/config` | `src/setup/config.rs` | TOML config types with serde serialization |
| `setup/detect` | `src/setup/detect.rs` | System detection (devices, subvols, init, packages) |
| `setup/env_export` | `src/setup/env_export.rs` | Shell-sourceable `DAS_*` variables the bash scripts read (`btrdasd config dump-env`) |
| `setup/templates` | `src/setup/templates.rs` | Template engine for btrbk.conf, systemd, cron, scripts |
| `setup/installer` | `src/setup/installer.rs` | Install/uninstall/upgrade/check with manifest tracking |
| `setup/wizard` | `src/setup/wizard.rs` | 9-step interactive dialoguer wizard |

## Database Schema

### Tables

**snapshots** — One row per indexed BTRFS snapshot (schema version 4, stored in `PRAGMA user_version`; an older database is migrated on open).

| Column | Type | Description |
|--------|------|-------------|
| id | INTEGER PK | Auto-increment ID |
| name | TEXT | Snapshot name (e.g., `root`) |
| ts | TEXT | Timestamp (e.g., `20260221T0304`) |
| source | TEXT | Source directory (e.g., `nvme`) |
| path | TEXT UNIQUE | Full filesystem path to snapshot |
| indexed_at | INTEGER | Unix timestamp when indexed |

**files** — One row per unique file path within a series. Updated when file metadata changes.

| Column | Type | Description |
|--------|------|-------------|
| id | INTEGER PK | Auto-increment ID |
| series | TEXT | Subvolume series (snapshot name and source, joined by byte `0x1f`); `(series, path)` is unique |
| path | TEXT | Relative path within snapshot |
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

**snapshot_targets** — One row per (snapshot, target) pair: which backup targets physically hold a copy of a snapshot. Added in schema v3 (v0.7.19.0). A snapshot replicated to all three targets is still ONE logical snapshot indexed once, so without this table the index could name the snapshot holding a file but only ever point at the primary's path — useless in the disaster the recovery drives exist for. Presence is not derivable: the recovery targets keep `daily=7` against the primary's daily/weekly/monthly/yearly, so they hold a strict subset.

| Column | Type | Description |
|--------|------|-------------|
| snapshot_id | INTEGER FK | References snapshots(id) `ON DELETE CASCADE` — presence rows can never outlive their snapshot |
| target_root | TEXT | Mount root of the target holding this copy (e.g. `/mnt/backup-22tb`) |
| path | TEXT | Full path to the copy on that target |

Primary key is `(snapshot_id, target_root)`.

**backup_runs** — One row per recorded backup run (`timestamp`, `success`, `mode`, `snaps_created`, `snaps_sent`, `bytes_sent`, `duration_secs`, `errors`); written by `btrdasd backup record-run` and `btrdasd backup run`, read by the GUI history. A snapshot count the run could not take is NULL — shown as `unknown`, never as 0 — since schema 4, whose migration rebuilds this table alone in one transaction, keeping every row, id and value; the one exception is a negative count, which is no measurement and becomes NULL (`record-run` never accepted one, so only a hand-written row could hold it). **target_usage** — capacity samples per target label (`total_bytes`, `used_bytes`, `snapshot_count`) feeding the growth trend. Neither is touched by `reindex --rebuild`.

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
| `idx_files_series_path` | series, path (UNIQUE) | File dedup, scoped per subvolume series |
| `idx_snapshot_targets_snap` | snapshot_id | Which targets hold a given snapshot |
| `idx_spans_last` | last_snap | Span extension queries |

Two further indexes serve the history tables: `idx_backup_runs_ts` and `idx_target_usage_label_ts`.

## CLI Usage

### Index a backup target

```bash
btrdasd walk /mnt/backup-hdd
btrdasd walk /mnt/backup-hdd --db /custom/path/backup-index.db
```

`walk` mounts the configured targets for the duration (`--config`, default `/etc/das-backup/config.toml`), reconciles the index while they are still mounted, then unmounts them.

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
Reconciled: index already consistent
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
(1 results)
```

### List files in a snapshot

```bash
btrdasd list nvme/root.20260221T0304
btrdasd list "root.20260221*"       # pattern matching
```

The listing ends with a `(N files)` line.

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
sudo btrdasd setup                  # Fresh install (9-step wizard)
sudo btrdasd setup --modify         # Re-open wizard with existing config
sudo btrdasd setup --upgrade        # Regenerate files after binary update
sudo btrdasd setup --uninstall      # Remove all generated files
sudo btrdasd setup --check          # Validate config and dependencies
```

See [INSTALL.md](INSTALL.md) for full installer documentation.

### Reconcile the index against disk

```bash
sudo btrdasd reconcile --dry-run     # Report what would be removed, change nothing
sudo btrdasd reconcile               # Prune index rows for snapshots gone from disk
sudo btrdasd reconcile --repair      # First delete spans whose endpoints name missing
                                     # snapshots — REQUIRED on an index carrying such
                                     # rows, since foreign keys are enforced and they
                                     # cannot be rewritten
sudo btrdasd reconcile --forget-root /mnt/old-target
                                     # Drop all rows recorded under a mount path that is
                                     # no longer a configured target (e.g. retired by a
                                     # rename). Such rows can never be reconciled, because
                                     # that root will never be mounted again. Refuses a
                                     # root that IS still configured
```

Reconcile is **mountpoint-gated**: it will not prune rows for a target that is not currently
mounted, because an unmounted target is indistinguishable from an empty one and pruning on
that basis would discard a valid index.

### Subvolume sync and expiry

```bash
sudo btrdasd subvol sync --dry-run   # Print the adopt/retire plan, change nothing
sudo btrdasd subvol sync             # Adopt new subvolumes, retire vanished ones
sudo btrdasd subvol expire --dry-run # Show retired series whose backups are past their window
```

`sync` adopts every subvolume that exists on a source volume and is not excluded by `[subvolumes].exclude`, marks entries whose subvolume has gone as retired, and regenerates `/etc/btrbk/btrbk.conf` whenever it differs from `config.toml`. `expire` deletes a retired series' backups per target once that target's longest retention window has passed. `backup-run.sh` runs sync before btrbk and expire after it; `btrdasd backup run` and a GUI-started backup run sync only. Rules and keep-conditions: [INSTALL.md](INSTALL.md) `[subvolumes]`.

### Delete snapshots and rebuild the index

```bash
btrdasd forget 'Projects-old-name' --dry-run   # Delete obsolete snapshots whose series name matches
btrdasd purge '*id_rsa' --dry-run              # Delete every snapshot containing a matching file path
sudo btrdasd reindex --rebuild                 # Discard the index and rebuild it (backup history is kept)
```

`forget` refuses a series that `btrbk.conf` still lists as live. `purge` deletes whole snapshots, because a file cannot be removed from inside one; affected series re-send in full on the next backup.

### Default database path

All commands use `--db /var/lib/das-backup/backup-index.db` by default. Override with `--db <path>`.

## Integration with backup-run.sh

The backup script calls `btrdasd` after btrbk creates snapshots:

```bash
run_indexer() {
    local indexer="${BTRDASD_BIN:-/usr/bin/btrdasd}"
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
- CMake build options

## Development

```bash
cd indexer
cargo test --features dbus   # 856 run (864 listed with `-- --list` incl. 8 manual root-only loopback tests, 2026-10-02): lib and CLI unit tests, helper and integration suites
cargo clippy --features dbus # Lint check
cargo fmt --check            # Format check
cargo audit                  # Security audit
cd .. && cmake --build build # Release build (btrdasd is about 6.9 MB, i.e. 6,895,832 bytes, as installed)
```

Build through CMake, not a bare `cargo build`: CMake sets `--target-dir build/cargo-target/`, where the install step looks for the binaries. Mutation testing gates every push (`.github/workflows/mutants.yml`).

## Design Decisions

1. **Rust over C++**: Chosen for memory safety in the data-intensive indexing pipeline. The KDE Plasma GUI remains C++/Qt6 since KF6 classes require native C++ integration.

2. **Bundled SQLite**: The `rusqlite` crate uses the `bundled` feature to compile SQLite from source, guaranteeing FTS5 availability regardless of system SQLite configuration.

3. **Span-based storage**: Instead of a naive file-per-snapshot model (which would create millions of rows), spans compress unchanged file presence across consecutive snapshots into single rows.

4. **WAL journal mode**: Enables concurrent reads during writes, important when the GUI reads the database while the indexer is running.

5. **FTS5 with triggers**: The FTS5 virtual table is synced automatically via INSERT/UPDATE/DELETE triggers, ensuring the search index is always consistent.

6. **Config-driven installer**: The TOML configuration is the single source of truth. All generated files (btrbk.conf, systemd units, scripts) are reproducible from the config, enabling clean upgrades and uninstalls via manifest tracking.

7. **Distro-agnostic design**: The installer detects the init system (systemd/sysvinit/OpenRC) and package manager at runtime, generating appropriate service files or cron entries for the host platform.
