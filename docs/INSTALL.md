# DAS-Backup-Manager — Installation Guide

**Version**: 0.7.12.3

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
| rsync | system | Manual disaster-recovery restores (see [Disaster Recovery Guide](DISASTER-RECOVERY-GUIDE.md)) — not used by any automated backup path |
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

The wizard auto-detects the init system, package manager, and installed dependencies
before it starts, then walks through the following on-screen steps (numbered `[1/10]`
through `[10/10]` — step 4 was the ESP-mirroring step, removed 2026-04-12 along with all
ESP sync code; the wizard's step counter still skips straight from `[3/10]` to `[5/10]`):

1. **Checking Dependencies** `[1/10]` — verifies btrbk, btrfs, smartctl, etc. against the
   auto-detected system info
2. **Backup Sources (BTRFS Subvolumes)** `[2/10]` — choose BTRFS subvolumes to back up
3. **Backup Targets** `[3/10]` — choose backup destination drives
4. **Retention Policy** `[5/10]` — weekly and monthly snapshot counts per target
5. **Backup Schedule** `[6/10]` — incremental and full backup times
6. **Email Notifications** `[7/10]` — optional SMTP configuration (reads
   `~/.config/pbridge.conf`)
7. **Install Location** `[8/10]` — binary/script install prefix
8. **KDE Plasma GUI** `[9/10]` — GUI desktop entry install toggle
9. **Review Configuration** `[10/10]` — shows generated config, writes files

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
- systemd/cron units (backup, scrub, doctor)
- Generated backup scripts

`~/.config/pbridge.conf` is never touched — it is a user-curated credential file the
installer only reads from, never generates or removes.

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

# Email credentials are read directly from ~/.config/pbridge.conf — no
# project-local email config file is required. See the Protonmail Bridge
# section of ~/.claude/rules/infrastructure.md.

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
| `version` | string | `"0.7.12"` | Config format version (tracks `CARGO_PKG_VERSION`, the 3-part semver) |
| `install_prefix` | string | `"/usr/local"` | Binary and script install prefix |
| `db_path` | string | `"/var/lib/das-backup/backup-index.db"` | SQLite database path |
| `log_file` | string | `"/var/log/das-backup.log"` | Backup log path |
| `growth_log` | string | `"/var/lib/das-backup/growth.log"` | Capacity growth trend log path |
| `last_report` | string | `"/var/lib/das-backup/last-report.txt"` | Most recent email report body, cached for the GUI |
| `btrbk_conf` | string | `"/etc/btrbk/btrbk.conf"` | btrbk config path |

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
| `role` | enum | `"primary"` or `"mirror"` |
| `retention.weekly` | u32 | Number of weekly snapshots to retain |
| `retention.monthly` | u32 | Number of monthly snapshots to retain |

### `[boot]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable boot subvolume archival (archive-then-recreate on `--full` runs) |
| `subvolumes` | string[] | `["@", "@home"]` | Subvolumes archived and recreated |
| `archive_retention_days` | u32 | `60` | Days to retain `@.archive.*`/`@home.archive.*` snapshots before `boot-archive-cleanup.sh` prunes them |

### `[doctor]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `exclude` | string[] | `[]` | Extra glob patterns excluded from `btrdasd doctor --check-drift` reporting, on top of the built-in exclusions |

**Removed section**: `[esp]` (enabled, mirror, partitions, mount_points, hooks.enabled,
hooks.type) — ESP/boot partition mirroring was removed from the codebase on 2026-04-10
(orphan pacman hook generator) and 2026-04-12 (remaining `Esp` struct and `sync_esp()`);
see `.claude/rules/esp-safety.md`. Old `config.toml` files with a leftover `[esp]` section
are silently ignored by serde on load.

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

### `[scrub]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable `das-scrub.timer` (the scrub engine itself always allows manual `btrdasd scrub run`, warning-only when disabled) |
| `on_calendar` | string | `"*-*-01 03:05:00"` | systemd `OnCalendar=` expression consumed verbatim by `das-scrub.timer` (monthly, 03:05 on the 1st — deliberately trails the 03:00 backup so the maintenance lock starts the scrub immediately after it finishes) |
| `targets` | string[] | `["primary-22tb", "system-recovery-A-2tb", "system-recovery-B-2tb"]` | `[[target]].label` values scrubbed sequentially in list order |
| `warn_age_days` | u32 | `45` | Days since a target's last successful scrub before health checks warn |
| `fail_age_days` | u32 | `75` | Days since a target's last successful scrub before health checks fail |

## Generated Files

The installer creates the following files (tracked in `/etc/das-backup/.manifest`):

| File | Purpose |
|------|---------|
| `/etc/das-backup/config.toml` | Master configuration |
| `/etc/btrbk/btrbk.conf` | btrbk snapshot configuration |
| `${prefix}/lib/das-backup/backup-run.sh` | Real production backup orchestrator script, installed flat (same layout `cmake --install` uses — no `scripts/` subdirectory, no wrapper) |
| `${prefix}/lib/das-backup/backup-verify.sh` | Real production drive-verification script, installed flat |
| `${prefix}/lib/das-backup/boot-archive-cleanup.sh` | Real production archive-pruner script, installed flat |
| `/etc/systemd/system/das-backup.service` | Incremental backup service (systemd) |
| `/etc/systemd/system/das-backup.timer` | Incremental backup timer (systemd) |
| `/etc/systemd/system/das-backup-full.service` | Full backup service (systemd) |
| `/etc/systemd/system/das-backup-full.timer` | Full backup timer (systemd) |
| `/etc/systemd/system/das-scrub.service` | Scheduled BTRFS scrub service (systemd) — runs `btrdasd scrub run` |
| `/etc/systemd/system/das-scrub.timer` | Scheduled BTRFS scrub timer (systemd) — `OnCalendar` from `[scrub].on_calendar`, enabled only when `[scrub].enabled = true` |
| `/etc/systemd/system/das-backup-doctor.service` | Subvolume drift detector service (systemd) — runs `btrdasd doctor --check-drift --email` |
| `/etc/systemd/system/das-backup-doctor.timer` | Subvolume drift detector timer (systemd) — fixed `Sun 02:00`, always enabled |

`${prefix}` is `[general].install_prefix` (default `/usr/local`, `/usr` on the live system
here). SMTP credentials are **not** a generated file — `~/.config/pbridge.conf` is a
user-curated file the installer reads from but never writes (see the Protonmail Bridge
section of `~/.claude/rules/infrastructure.md`). ESP pacman hook generation was removed
2026-04-10; no ESP hook file is ever generated.

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
