# DAS Backup Manager

[![CodeFactor](https://www.codefactor.io/repository/github/theboscoclub/DAS-Backup-Manager/badge)](https://www.codefactor.io/repository/github/theboscoclub/DAS-Backup-Manager)

**Version**: 0.4.0

DAS backup manager with btrbk integration, SQLite FTS5 content indexing, KDE Plasma GUI, and an interactive installer for the full backup pipeline.

## Features

- **btrbk Backup Orchestration** — Nightly incremental BTRFS snapshot backups to DAS enclosure
- **Triple-Target Architecture** — 22TB primary backup + 2x 2TB bootable recovery drives
- **Boot Subvolume Archival** — Archives old boot subvolumes with timestamps (1-year retention)
- **Email Reports** — Automated backup status reports with throughput metrics and SMART status
- **ButteredDASD Content Indexer** (`btrdasd`) — Rust CLI with SQLite FTS5 database tracking every file across all snapshots
- **KDE Plasma GUI** (`btrdasd-gui`) — Native Qt6/KF6 application for searching, browsing, and restoring files from backup snapshots
- **Interactive Installer** (`btrdasd setup`) — 10-step wizard with 5 modes: install, modify, upgrade, uninstall, check
- **Distro-Agnostic** — Supports systemd, sysvinit, and OpenRC init systems
- **Docker Support** — Headless `btrdasd` CLI in a container

## Components

| Component | Description | Status |
|-----------|-------------|--------|
| `scripts/backup-run.sh` | btrbk backup orchestrator with email reporting | Active (v3.1.0) |
| `scripts/backup-verify.sh` | DAS drive health and btrbk status verification | Active (v2.0.0) |
| `scripts/boot-archive-cleanup.sh` | Prune old boot subvolume archives | Active (v1.0.0) |
| `scripts/das-partition-drives.sh` | DAS drive partitioning utility | Active (v1.0.0) |
| `scripts/install-backup-timer.sh` | systemd timer installer | Active |
| `config/btrbk.conf` | Reference btrbk configuration | Active |
| `indexer/` | ButteredDASD (`btrdasd`) — Rust content indexer + setup wizard | Active (v0.4.0) |
| `gui/` | Qt6/KDE Plasma backup browser and restore application | Active (v0.4.0) |
| `Dockerfile` | Multi-stage Docker build for headless btrdasd CLI | Active |

## Project Structure

```
DAS-Backup-Manager/
├── scripts/           # Shell scripts (backup, verify, cleanup, partition)
├── config/            # btrbk.conf, email config template
├── indexer/           # ButteredDASD (btrdasd) — Rust indexer + installer
│   └── src/setup/     # Interactive installer (wizard, templates, detection)
├── gui/               # Qt6/KDE Plasma GUI (12 C++ components)
│   ├── src/           # MainWindow, Database, models, timeline, restore
│   └── tests/         # QTest suites (database, snapshotmodel, filemodel, searchmodel)
├── docs/              # Architecture, installation, dependencies, recovery
├── Dockerfile         # Headless CLI container
└── CMakeLists.txt     # Build system (BUILD_GUI, BUILD_INDEXER options)
```

## Requirements

- Linux with BTRFS filesystem support
- btrbk 0.32+, smartmontools, zsh 5.9+
- Rust 1.87+ with Cargo (for building the indexer and installer)
- **Optional**: Qt6 6.6+, KDE Frameworks 6.0+, CMake 3.25+ (for the GUI)

## Installation

### Recommended: Interactive Setup

```bash
# Build and install the binary
cd indexer && cargo build --release
sudo cp target/release/btrdasd /usr/local/bin/

# Run the interactive setup wizard
sudo btrdasd setup
```

The wizard configures backup sources, targets, retention, scheduling, email, and ESP mirroring — then generates all configuration files and enables timers.

See [docs/INSTALL.md](docs/INSTALL.md) for all installation methods including manual setup, Docker, and CMake build options.

### Quick Docker

```bash
docker build -t btrdasd .
docker run --rm btrdasd --version
```

## Design Philosophy

- **Security-first**: Rust for the data pipeline (no buffer overflows, use-after-free, or data races). C++20 RAII with `-Werror` for the GUI. Exclusive prepared statements for all SQL.
- **Memory safety**: Single `unsafe` call in the entire Rust codebase (`libc::geteuid`). No raw pointers in C++ GUI code.
- **Efficiency**: Span-based deduplication compresses file presence across snapshots. Incremental indexing skips already-processed snapshots. Six targeted performance indexes.
- **Stability**: Indexing errors never abort backups (soft-fail). GUI gracefully handles missing or locked databases.
- **Privacy**: File metadata only — no file contents are ever read or stored. No telemetry or network connections.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — System design, data flow, security decisions, encryption assessment
- [Installation Guide](docs/INSTALL.md) — All installation methods, config reference, Docker
- [ButteredDASD Indexer](docs/BUTTERED-DASD.md) — CLI usage, schema, span logic, development
- [Dependencies](docs/DEPENDENCIES.md) — Rust crates, system deps, build deps, GUI deps
- [Offline Backup Plan](docs/OFFLINE-BACKUP-PLAN.md) — Capacity planning, drive allocation
- [Disaster Recovery Guide](docs/DISASTER-RECOVERY-GUIDE.md) — Step-by-step recovery procedures
- [Storage Architecture](docs/STORAGE-ARCHITECTURE-AND-RECOVERY.md) — Full system storage reference
- [DAS Bay Mapping](docs/DAS-BAY-MAPPING.md) — Physical drive locations and serial numbers

## License

MIT — See [LICENSE](LICENSE) for details.
