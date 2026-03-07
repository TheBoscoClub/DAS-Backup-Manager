# DAS-Backup-Manager — Installation Guide

**Version**: 0.7.9

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
| util-linux | system | Block device detection (`lsblk`), mount/umount |
| bash | 4.0+ | Runtime shell for backup scripts |

### Optional (for features)

| Dependency | Version | Purpose |
|-----------|---------|---------|
| s-nail (mailx) | system | Email backup reports (when email reporting enabled) |
| rsync | system | ESP/boot partition mirroring (when ESP sync enabled) |
| mbuffer | system | Buffered btrbk stream transfers (improves throughput) |

### Optional (for GUI)

| Dependency | Version | Purpose |
|-----------|---------|---------|
| Qt6 | 6.6+ (tested 6.10.2) | UI framework |
| Qt6 Charts | 6.6+ (tested 6.10.2) | Growth trendline chart (`qt6-charts` package) |
| KDE Frameworks 6 | 6.0+ (tested 6.23.0) | KXmlGuiWindow, KIO, KAboutData, Notifications, StatusNotifierItem |
| CMake | 3.25+ (tested 4.2.3) | Build system for GUI component |
| Extra CMake Modules (ECM) | ships with KF6 | KDE-specific CMake macros |

## Quick Start — Full Build (CLI + GUI + Helper)

The recommended installation method builds all components and runs the setup wizard:

```bash
# 1. Build everything (CLI, D-Bus helper, FFI library, KDE GUI)
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build

# 2. Install all components (binaries, scripts, systemd, D-Bus, polkit, man page, icons)
sudo cmake --install build

# 3. Run the interactive setup wizard
sudo btrdasd setup
```

This installs: `btrdasd` (CLI), `btrdasd-gui` (KDE GUI), `btrdasd-helper` (D-Bus daemon), `libbuttered_dasd_ffi.so` (FFI library), backup scripts, systemd units, D-Bus/polkit configs, shell completions, man page, and desktop entry.

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

### Full Uninstall (everything)

```bash
sudo btrdasd setup --uninstall-all
```

Removes all generated files (same as `--uninstall`), then also removes cmake-installed components: binaries (`btrdasd`, `btrdasd-gui`, `btrdasd-helper`), FFI library, D-Bus configs, polkit policy, systemd units, man page, shell completions, desktop entry, and icon. Prompts whether to remove the backup database.

### Non-Interactive Mode (`--force`)

Add `--force` to any setup mode for unattended operation:

```bash
# Uninstall everything, keep database
sudo btrdasd setup --uninstall-all --force

# Reinstall from existing config
sudo btrdasd setup --force

# Upgrade without prompts
sudo btrdasd setup --upgrade --force
```

The `--force` flag skips all interactive prompts and **never removes or overwrites the backup database**. Requires an existing config for install mode (use the interactive wizard for first-time setup).

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

For users who prefer manual configuration without the setup wizard:

```bash
# Build and install all components
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
sudo cmake --install build

# Create database directory
sudo mkdir -p /var/lib/das-backup

# Configure btrbk manually
sudo cp config/btrbk.conf /etc/btrbk/btrbk.conf
sudo vim /etc/btrbk/btrbk.conf  # edit for your drives

# Set up email config
sudo cp config/das-backup-email.conf.example /etc/das-backup-email.conf
sudo chmod 600 /etc/das-backup-email.conf
sudo vim /etc/das-backup-email.conf  # add SMTP credentials

# Enable systemd timers
sudo systemctl enable --now das-backup.timer das-backup-full.timer
```

## CLI-Only Build (no GUI dependencies)

If you don't have Qt6/KF6 installed or don't need the GUI:

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_GUI=OFF -DBUILD_FFI=OFF
cmake --build build
sudo cmake --install build
```

This still installs the CLI, D-Bus helper, backup scripts, systemd units, polkit policy, and man page — everything except the GUI and FFI library.

## CMake Build Options

| Option | Default | Description |
|--------|---------|-------------|
| `BUILD_GUI` | `ON` | Build the KDE Plasma GUI (requires Qt6/KF6) |
| `BUILD_INDEXER` | `ON` | Build the `btrdasd` Rust binary via cargo |
| `BUILD_HELPER` | `ON` | Build the `btrdasd-helper` D-Bus daemon and install polkit/D-Bus config |
| `BUILD_FFI` | `ON` | Build `libbuttered_dasd_ffi.so` C-ABI shared library (for GUI) |
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

## Distribution Packages

Native packaging recipes are included under `packaging/` and build-tested on their respective distributions before each release.

| Distribution | Format | Directory | GUI Support |
|---|---|---|---|
| Arch Linux / CachyOS | PKGBUILD (`makepkg`) | `packaging/arch/` | Full |
| Debian 13+ / Ubuntu 24.10+ | dpkg (`dpkg-buildpackage`) | `packaging/debian/` | Full (KF6 required) |
| Fedora 43+ | RPM (`rpmbuild`) | `packaging/fedora/` | Full |
| Flatpak | Flatpak manifest | `packaging/flatpak/` | Full |
| Snap | snapcraft | `packaging/snap/` | Full |
| Ubuntu 24.04 LTS | cmake (CLI-only) | — | No (KF6 unavailable) |

**Arch Linux example:**

```bash
cd packaging/arch
makepkg -si
```

**Minimum Rust version**: 1.87+ (for Rust edition 2024 and `let_chains`). Distributions shipping older Rust (e.g., Debian 13 with 1.85) require [rustup](https://rustup.rs/) for compilation.


## Configuration Reference

The installer generates `/etc/das-backup/config.toml` with the following sections:

### `[general]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `version` | string | `"0.7.9"` | Config format version |
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
