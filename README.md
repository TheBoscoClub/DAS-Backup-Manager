# DAS Backup Manager

[![CodeFactor](https://www.codefactor.io/repository/github/theboscoclub/DAS-Backup-Manager/badge)](https://www.codefactor.io/repository/github/theboscoclub/DAS-Backup-Manager)

**Version**: 0.1.0-dev

DAS backup manager with btrbk integration, SQLite FTS5 content indexing, and a KDE Plasma GUI for browsing and restoring files from BTRFS snapshots.

## Features

- **btrbk Backup Orchestration** — Weekly full and daily incremental BTRFS snapshot backups to offline DAS enclosure
- **Boot Subvolume Archival** — Archives old boot subvolumes with timestamps instead of deleting them (1-year retention)
- **Content Indexer** — SQLite FTS5 database tracking every file across all snapshots, including deletions
- **KDE Plasma GUI** — Native Qt6/C++ application for searching, browsing, and restoring files from any backup snapshot
- **Email Reports** — Automated backup status reports with throughput metrics

## Components

| Component | Description | Status |
|-----------|-------------|--------|
| `scripts/backup-run.sh` | btrbk backup orchestrator with email reporting | Migrated (v3.1.0) |
| `scripts/backup-verify.sh` | DAS drive health and btrbk status verification | Migrated (v2.0.0) |
| `indexer/` | C++ SQLite FTS5 content indexer CLI | Planned |
| `gui/` | Qt6/KDE Plasma backup browser and restore app | Planned |

## Requirements

- CachyOS (or Arch-based) Linux
- BTRFS filesystem with btrbk configured
- Qt6 6.10+, KDE Frameworks 6.23+, CMake 4.2+
- SQLite 3.51+ with FTS5 extension
- DAS enclosure with BTRFS-formatted drives

## Building

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
sudo cmake --install build
```

## License

GPL-3.0 — See [LICENSE](LICENSE) for details.
