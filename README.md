# DAS Backup Manager

[![CodeFactor](https://www.codefactor.io/repository/github/theboscoclub/DAS-Backup-Manager/badge)](https://www.codefactor.io/repository/github/theboscoclub/DAS-Backup-Manager)

**Version**: 0.6.0

DAS backup manager with btrbk integration, SQLite FTS5 content indexing, KDE Plasma GUI with full backup management, D-Bus privilege escalation, and an interactive installer for the full backup pipeline.

## Scope

This project manages backups to **Direct-Attached Storage (DAS)** using the **BTRFS** filesystem. That's it. That's the scope.

The following are permanently out of scope and will never be added:

- **NAS** (Network-Attached Storage)
- **SAN** (Storage Area Network)
- **Cloud storage** (S3, Azure Blob, GCS, Backblaze, etc.)
- **Any filesystem other than BTRFS** (ext4, XFS, ZFS, NTFS, etc.)

This is not a general-purpose backup tool. It is a DAS + BTRFS backup tool. If you need support for other storage architectures or filesystems, you are welcome to write your own application that covers whichever and however many storage backends your heart desires.

That said, suggestions, recommendations, and requests that fall within this narrow scope are very welcome and will be happily entertained.

## Features

- **btrbk Backup Orchestration** — Nightly incremental BTRFS snapshot backups to DAS enclosure
- **Multi-Target Architecture** — Configurable primary, mirror, and ESP-sync roles across any number of DAS drives
- **Boot Subvolume Archival** — Archives old boot subvolumes with timestamps (configurable retention)
- **Email Reports** — Automated backup status reports with throughput metrics and SMART status
- **ButteredDASD Content Indexer** (`buttered_dasd` library + `btrdasd` CLI) — Rust library and CLI with SQLite FTS5 database tracking every file across all snapshots
- **D-Bus Privileged Helper** (`btrdasd-helper`) — polkit-authorized daemon for privilege-escalated operations (backup, restore, config, schedule, health)
- **FFI Bridge** (`libbuttered_dasd_ffi.so`) — C-ABI shared library for GUI access to Rust library functions
- **KDE Plasma GUI** (`btrdasd-gui`) — Native Qt6/KF6 full backup management application with sidebar navigation, Dolphin-style file browser, backup operations, health dashboard, config editor, first-run wizard, desktop notifications, and system tray
- **Interactive Installer** (`btrdasd setup`) — 10-step wizard with 5 modes: install, modify, upgrade, uninstall, check
- **Shell Completions** — `btrdasd completions` generates completions for bash, zsh, fish, elvish, and PowerShell
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
| `indexer/` | ButteredDASD (`buttered_dasd` lib + `btrdasd` CLI + `btrdasd-helper` D-Bus daemon + FFI cdylib) | Active (v0.6.0) |
| `gui/` | Qt6/KDE Plasma full backup management GUI (19 C++ components, 4 test suites) | Active (v0.6.0) |
| `dbus/` | D-Bus system bus configuration and service activation files | Active (v0.6.0) |
| `polkit/` | Polkit policy for privilege escalation (backup, restore, config, health) | Active (v0.6.0) |
| `Dockerfile` | Multi-stage Docker build for headless btrdasd CLI | Active |

## Project Structure

```
DAS-Backup-Manager/
├── scripts/           # Shell scripts (backup, verify, cleanup, partition)
├── config/            # btrbk.conf, email config template
├── indexer/           # ButteredDASD — Rust library + CLI + D-Bus helper + FFI
│   ├── src/           # Library modules (11): backup, config, db, health, indexer, progress, report, restore, scanner, schedule, subvol
│   ├── src/setup/     # Binary-only: interactive installer (wizard, templates, detection)
│   ├── src/bin/       # btrdasd-helper D-Bus daemon
│   ├── src/ffi.rs     # C-ABI FFI bridge (extern "C" functions)
│   ├── include/       # C header (btrdasd_ffi.h)
│   └── completions/   # Shell completion installation instructions
├── gui/               # Qt6/KDE Plasma GUI (19 C++ components)
│   ├── src/           # MainWindow, Sidebar, DBusClient, panels, dialogs, models
│   └── tests/         # QTest suites (database, snapshotmodel, filemodel, searchmodel)
├── dbus/              # D-Bus bus config and service activation
├── polkit/            # Polkit privilege escalation policy
├── docs/              # Architecture, installation, dependencies, recovery, man page
├── Dockerfile         # Headless CLI container
└── CMakeLists.txt     # Build system (BUILD_GUI, BUILD_INDEXER, BUILD_HELPER, BUILD_FFI)
```

## Minimum Requirements

- Linux with BTRFS support (kernel 5.15+)
- DAS enclosure (any manufacturer, any interface -- USB, Thunderbolt, eSATA)
- One or more BTRFS-formatted drives (any technology: HDD, SSD, NVMe)
- btrbk 0.32+, smartmontools, zsh 5.9+
- Rust 1.87+ with Cargo (for building the indexer and installer)
- **Optional**: Qt6 6.6+ (with Qt6::DBus, Qt6::Charts), KDE Frameworks 6.0+ (with KNotifications, KStatusNotifierItem), CMake 3.25+ (for the GUI)

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
- **Memory safety**: Minimal `unsafe` in Rust (libc calls and FFI boundary). No raw pointers in C++ GUI code. Smart pointers exclusively.
- **Efficiency**: Span-based deduplication compresses file presence across snapshots. Incremental indexing skips already-processed snapshots. Six targeted performance indexes.
- **Stability**: Indexing errors never abort backups (soft-fail). GUI gracefully handles missing or locked databases.
- **Privacy**: File metadata only — no file contents are ever read or stored. No telemetry or network connections.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — System design, data flow, security decisions, encryption assessment
- [Installation Guide](docs/INSTALL.md) — All installation methods, config reference, Docker
- [ButteredDASD Indexer](docs/BUTTERED-DASD.md) — CLI usage, schema, span logic, development
- [Dependencies](docs/DEPENDENCIES.md) — Rust crates, system deps, build deps, GUI deps
- [Backup Planning](docs/OFFLINE-BACKUP-PLAN.md) — Capacity planning, drive selection, retention worksheet
- [Disaster Recovery Guide](docs/DISASTER-RECOVERY-GUIDE.md) — Step-by-step recovery procedures
- [Storage Architecture & Recovery](docs/STORAGE-ARCHITECTURE-AND-RECOVERY.md) — BTRFS RAID concepts, failure detection, recovery procedures
- [DAS Bay Mapping](docs/DAS-BAY-MAPPING.md) — How to map and document physical drive locations
- [Reference Examples](docs/examples/) — Author's hardware setup as a worked example

## License

MIT — See [LICENSE](LICENSE) for details.
