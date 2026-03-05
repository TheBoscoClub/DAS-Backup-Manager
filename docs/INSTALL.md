# DAS-Backup-Manager — Installation Guide

**Version**: 0.7.0

## Before You Begin

### Minimum Requirements

- Linux with BTRFS support (kernel 5.15+)
- DAS enclosure (any manufacturer, any interface -- USB, Thunderbolt, eSATA) in JBOD mode
- One or more BTRFS-formatted drives (any technology: HDD, SSD, NVMe)
- btrbk 0.32+, smartmontools
- Rust 1.87+ with Cargo (for building btrdasd)

### Planning Your Backup

Before installing, work through the [Backup Planning Guide](OFFLINE-BACKUP-PLAN.md) to determine:

1. **What to back up** -- which BTRFS subvolumes contain irreplaceable data
2. **Retention depth** -- how many weekly/monthly snapshots to keep
3. **Target capacity** -- how much storage you need on your DAS drives
4. **Drive roles** -- which drives serve as primary backup, bootable recovery, or general storage

The planning worksheet in that guide helps you estimate capacity requirements before you buy hardware.

## Prerequisites

### Required

| Dependency | Version | Purpose |
|-----------|---------|---------|
| Rust toolchain | **1.87+** | Edition 2024 with `let_chains` (stable since 1.87) |
| C compiler | gcc or clang | Required by `libsqlite3-sys` to build bundled SQLite |
| btrbk | 0.32+ | BTRFS snapshot creation and send/receive |
| btrfs-progs | system | BTRFS subvolume operations |
| smartmontools | system | Drive health and serial number detection |
| bash | 4.0+ | Runtime shell for backup scripts |

### Optional (for GUI)

| Dependency | Version | Purpose |
|-----------|---------|---------|
| Qt6 | 6.6+ (tested 6.10.2) | UI framework |
| KDE Frameworks 6 | 6.0+ (tested 6.23.0) | KXmlGuiWindow, KIO, KAboutData |
| CMake | 3.25+ (tested 4.2.3) | Build system for GUI component |
| Extra CMake Modules (ECM) | ships with KF6 | KDE-specific CMake macros |

## Quick Start with `btrdasd setup`

The recommended installation method uses the interactive setup wizard:

```bash
# 1. Build the indexer
cd indexer
cargo build --release

# 2. Install the binary
sudo cp target/release/btrdasd /usr/local/bin/

# 3. Run the interactive setup wizard
sudo btrdasd setup
```

The wizard walks through 10 configuration steps:

1. **Init system detection** — systemd, sysvinit, or OpenRC
2. **Package manager detection** — pacman, apt, dnf, zypper
3. **Dependency check** — verifies btrbk, btrfs, smartctl, etc.
4. **Source selection** — choose BTRFS subvolumes to back up
5. **Target selection** — choose backup destination drives
6. **Retention policy** — weekly and monthly snapshot counts per target
7. **Schedule** — incremental and full backup times
8. **ESP mirroring** — optional boot partition synchronization
9. **Email reports** — optional SMTP configuration
10. **Review and install** — shows generated config, writes files

## Installer Modes

### Fresh Install (default)

```bash
sudo btrdasd setup
```

Runs the full 10-step wizard, generates all configuration files, and enables backup timers.

### Modify Existing Config

```bash
sudo btrdasd setup --modify
```

Re-opens the wizard with your current configuration pre-filled from `/etc/das-backup/config.toml`. Change any settings, then regenerate files.

### Upgrade After Binary Update

```bash
sudo btrdasd setup --upgrade
```

Regenerates all files from the existing config without re-running the wizard. Use this after updating the `btrdasd` binary to ensure generated scripts match the new version.

### Uninstall

```bash
sudo btrdasd setup --uninstall
```

Removes all files listed in the install manifest (`/etc/das-backup/.manifest`):
- Generated btrbk.conf
- systemd/cron units
- Generated backup scripts
- Email configuration
- ESP hooks

Prompts whether to also remove the backup database at `/var/lib/das-backup/backup-index.db`. The TOML config file is preserved for potential reinstallation.

### Check Installation

```bash
sudo btrdasd setup --check
```

Validates the current installation without changing anything:
- Loads and validates `/etc/das-backup/config.toml`
- Checks all dependencies are installed
- Verifies all manifest files exist on disk
- Reports any issues found

## Manual Installation (without wizard)

For users who prefer manual configuration:

```bash
# Build the Rust indexer
cd indexer
cargo build --release
sudo cp target/release/btrdasd /usr/local/bin/

# Create database directory
sudo mkdir -p /var/lib/das-backup

# Install reference scripts via CMake
cd ..
cmake -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_GUI=OFF
cmake --build build
sudo cmake --install build

# Configure btrbk manually
sudo cp config/btrbk.conf /etc/btrbk/btrbk.conf
sudo vim /etc/btrbk/btrbk.conf  # edit for your drives

# Set up email config
sudo cp config/das-backup-email.conf.example /etc/das-backup-email.conf
sudo chmod 600 /etc/das-backup-email.conf
sudo vim /etc/das-backup-email.conf  # add SMTP credentials

# Install and enable systemd timers
sudo scripts/install-backup-timer.sh
sudo systemctl start das-backup.timer
```

## Building the GUI

```bash
# Full build (indexer + GUI)
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build

# Install
sudo cmake --install build
```

The GUI binary installs to `${CMAKE_INSTALL_PREFIX}/bin/btrdasd-gui` with a desktop entry for KDE application menus.

## CMake Build Options

| Option | Default | Description |
|--------|---------|-------------|
| `BUILD_GUI` | `ON` | Build the KDE Plasma GUI (requires Qt6/KF6) |
| `BUILD_INDEXER` | `ON` | Build the `btrdasd` Rust binary via cargo |
| `BUILD_HELPER` | `ON` | Build the `btrdasd-helper` D-Bus daemon and install polkit/D-Bus config |
| `BUILD_FFI` | `OFF` | Build `libbuttered_dasd_ffi.so` C-ABI shared library (for GUI) |
| `CMAKE_INSTALL_PREFIX` | `/usr/local` | Installation prefix for binaries and scripts |
| `CMAKE_BUILD_TYPE` | (unset) | `Release`, `RelWithDebInfo`, or `Debug` |

### CLI-Only Build (no GUI dependencies)

```bash
cmake -B build -DBUILD_GUI=OFF -DCMAKE_BUILD_TYPE=Release
cmake --build build
```

This skips Qt6/KF6 entirely — no GUI libraries needed on the system.

### Indexer-Only Build (cargo directly)

```bash
cd indexer
cargo build --release
# Binary at: indexer/target/release/btrdasd
```

## Docker

Build and run `btrdasd` in a container (headless CLI only):

```bash
# Build the image
docker build -t btrdasd .

# Run commands
docker run --rm btrdasd --version
docker run --rm btrdasd --help

# Index a mounted backup target
docker run --rm \
    -v /mnt/backup-hdd:/mnt/backup-hdd:ro \
    -v /var/lib/das-backup:/var/lib/das-backup \
    btrdasd walk /mnt/backup-hdd

# Search
docker run --rm \
    -v /var/lib/das-backup:/var/lib/das-backup:ro \
    btrdasd search "report.pdf"
```

The Dockerfile uses a multi-stage build:
- **Builder**: `rust:1.93-bookworm` — compiles the release binary
- **Runtime**: `debian:bookworm-slim` — minimal image with `btrfs-progs` and `smartmontools`

## Configuration Reference

The installer generates `/etc/das-backup/config.toml` with the following sections:

### `[general]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `version` | string | `"0.7.0"` | Config format version |
| `install_prefix` | string | `"/usr/local"` | Binary and script install prefix |
| `db_path` | string | `"/var/lib/das-backup/backup-index.db"` | SQLite database path |

### `[init]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `system` | enum | `"systemd"` | Init system: `systemd`, `sysvinit`, or `openrc` |

### `[schedule]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `incremental` | string | `"03:00"` | Daily incremental backup time |
| `full` | string | `"Sun 04:00"` | Weekly full backup day and time |
| `randomized_delay_min` | u32 | `30` | Random delay (minutes) to avoid I/O spikes |

### `[[source]]` (array)

| Field | Type | Description |
|-------|------|-------------|
| `label` | string | Human-readable name (e.g., `"nvme-root"`) |
| `volume` | string | BTRFS volume mount point (e.g., `"/.btrfs-nvme"`) |
| `subvolumes` | string[] | Subvolumes to snapshot (e.g., `["@", "@home"]`) |
| `device` | string | Block device path (e.g., `"/dev/nvme0n1p2"`) |

### `[[target]]` (array)

| Field | Type | Description |
|-------|------|-------------|
| `label` | string | Human-readable name (e.g., `"primary-22tb"`) |
| `serial` | string | Drive serial for identification |
| `mount` | string | Mount point (e.g., `"/mnt/backup-22tb"`) |
| `role` | enum | `"primary"`, `"mirror"`, or `"esp-sync"` |
| `retention.weekly` | u32 | Number of weekly snapshots to retain |
| `retention.monthly` | u32 | Number of monthly snapshots to retain |

### `[esp]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable ESP/boot partition mirroring |
| `mirror` | bool | `false` | Mirror ESP across multiple partitions |
| `partitions` | string[] | `[]` | ESP partition device paths |
| `mount_points` | string[] | `[]` | ESP mount points |
| `hooks.enabled` | bool | `false` | Generate package manager hooks |
| `hooks.type` | enum | `"none"` | `"pacman"`, `"apt"`, `"dnf"`, or `"none"` |

### `[email]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable email backup reports |
| `smtp_host` | string | `""` | SMTP server hostname |
| `smtp_port` | u16 | `0` | SMTP server port |
| `from` | string | `""` | Sender email address |
| `to` | string | `""` | Recipient email address |
| `auth` | enum | `"none"` | `"plain"`, `"starttls"`, or `"none"` |

### `[gui]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Install GUI desktop entry |

## Generated Files

The installer creates the following files (tracked in `/etc/das-backup/.manifest`):

| File | Purpose |
|------|---------|
| `/etc/das-backup/config.toml` | Master configuration |
| `/etc/btrbk/btrbk.conf` | btrbk snapshot configuration |
| `/etc/systemd/system/das-backup.service` | Incremental backup service (systemd) |
| `/etc/systemd/system/das-backup.timer` | Incremental backup timer (systemd) |
| `/etc/systemd/system/das-backup-full.service` | Full backup service (systemd) |
| `/etc/systemd/system/das-backup-full.timer` | Full backup timer (systemd) |
| `/usr/local/lib/das-backup/backup-run-generated.sh` | Generated backup script |
| `/etc/das-backup-email.conf` | SMTP credentials (mode 0600) |
| `/usr/share/libalpm/hooks/das-backup-esp.hook` | Pacman ESP hook (if enabled) |

For sysvinit/OpenRC systems, cron entries replace systemd units.

## Verifying the Installation

```bash
# Check installation status
sudo btrdasd setup --check

# Verify the binary
btrdasd --version

# Test database access
btrdasd info --db /var/lib/das-backup/backup-index.db

# Test a manual walk (if backup target is mounted)
btrdasd walk /mnt/backup-target

# Check systemd timers
systemctl list-timers das-backup*
```
