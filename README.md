# DAS Backup Manager

[![CodeFactor](https://www.codefactor.io/repository/github/theboscoclub/DAS-Backup-Manager/badge)](https://www.codefactor.io/repository/github/theboscoclub/DAS-Backup-Manager)

**Version**: 0.2.0

DAS backup manager with btrbk integration, SQLite FTS5 content indexing, and a KDE Plasma GUI for browsing and restoring files from BTRFS snapshots.

## Features

- **btrbk Backup Orchestration** — Nightly incremental BTRFS snapshot backups to TerraMaster D6-320 DAS enclosure
- **Triple-Target Architecture** — 22TB primary backup + 2x 2TB bootable recovery drives
- **Boot Subvolume Archival** — Archives old boot subvolumes with timestamps instead of deleting them (1-year retention)
- **Email Reports** — Automated backup status reports with throughput metrics, growth analysis, and SMART status
- **Content Indexer** — SQLite FTS5 database tracking every file across all snapshots (planned)
- **KDE Plasma GUI** — Native Qt6/C++ application for searching, browsing, and restoring files (planned)

## Components

| Component | Description | Status |
|-----------|-------------|--------|
| `scripts/backup-run.sh` | btrbk backup orchestrator with email reporting | Active (v3.1.0) |
| `scripts/backup-verify.sh` | DAS drive health and btrbk status verification | Active (v2.0.0) |
| `scripts/boot-archive-cleanup.sh` | Prune old boot subvolume archives | Active (v1.0.0) |
| `scripts/das-partition-drives.sh` | DAS drive partitioning utility | Active (v1.0.0) |
| `scripts/install-backup-timer.sh` | systemd timer installer | Active |
| `config/btrbk.conf` | Reference btrbk configuration | Active |
| `systemd/` | systemd service and timer units | Active |
| `docs/` | Architecture, recovery guides, bay mapping | Active |
| `indexer/` | C++ SQLite FTS5 content indexer CLI | Planned |
| `gui/` | Qt6/KDE Plasma backup browser and restore app | Planned |

## Project Structure

```
DAS-Backup-Manager/
├── scripts/           # Shell scripts (backup, verify, cleanup, partition)
├── config/            # btrbk.conf, email config template
├── systemd/           # systemd service and timer units
├── docs/              # Architecture docs, disaster recovery guide
├── indexer/           # (planned) C++ SQLite FTS5 content indexer
├── gui/               # (planned) Qt6/KDE Plasma GUI
└── CMakeLists.txt     # Build system
```

## Requirements

- CachyOS (or Arch-based) Linux with BTRFS
- btrbk 0.32+, smartmontools, s-nail (mailx)
- TerraMaster D6-320 DAS (or compatible USB JBOD enclosure)
- For future C++ components: Qt6 6.10+, KDE Frameworks 6.23+, CMake 3.25+, SQLite 3.51+ with FTS5

## Installation

```bash
# Build and install
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
sudo cmake --install build

# Install systemd timers
sudo scripts/install-backup-timer.sh

# Copy and configure email settings
sudo cp config/das-backup-email.conf.example /etc/das-backup-email.conf
sudo chmod 600 /etc/das-backup-email.conf
# Edit /etc/das-backup-email.conf with your Proton Bridge credentials

# Start nightly backups
sudo systemctl start das-backup.timer
```

## Documentation

- [Offline Backup Plan](docs/OFFLINE-BACKUP-PLAN.md) — Capacity planning, drive allocation, backup strategy
- [Disaster Recovery Guide](docs/DISASTER-RECOVERY-GUIDE.md) — Step-by-step recovery for NVMe/SSD/HDD failure
- [Storage Architecture](docs/STORAGE-ARCHITECTURE-AND-RECOVERY.md) — Full system storage reference
- [DAS Bay Mapping](docs/DAS-BAY-MAPPING.md) — Physical drive locations and serial numbers

## License

GPL-3.0 — See [LICENSE](LICENSE) for details.
