# DAS-Backup-Manager Installer Design

**Date:** 2026-02-21
**Status:** Approved

## Goal

A distro-agnostic, desktop-agnostic installer/setup tool that replaces all hardcoded paths, serials, and mount points with an externalized TOML config. Supports interactive subvolume/ESP selection, backup target configuration, optional email reports, optional GUI, install/upgrade/uninstall/modify modes, and scheduling configuration.

## Key Decisions

| Decision | Choice |
|---|---|
| Language | Rust (subcommand of `btrdasd`) |
| Config format | TOML at `/etc/das-backup/config.toml` |
| TUI framework | dialoguer + console crates |
| Init systems | systemd, sysvinit, OpenRC |
| Generated scripts | `#!/usr/bin/env bash` |
| Build default | GUI ON (`-DBUILD_GUI=OFF` to exclude) |
| License | MIT |
| Architecture | x86_64 official binaries; source open for others; Docker image for cross-platform |
| Package managers | pacman, apt, dnf, zypper, apk |

---

## Architecture

`btrdasd setup` is a subcommand of the existing `btrdasd` Rust binary. It runs an interactive wizard that:

1. **Detects** the system: BTRFS subvolumes, block devices, ESP partitions, DAS/USB enclosures, installed dependencies, init system, package manager
2. **Asks** the user what to back up, where to back up to, and optional features
3. **Generates** `/etc/das-backup/config.toml` as the single source of truth
4. **Renders** all operational files from embedded templates + config
5. **Installs** generated files to system locations, enables timers/cron, optionally installs deps

### Modes

| Subcommand | Purpose |
|---|---|
| `btrdasd setup` | Fresh install wizard |
| `btrdasd setup --modify` | Re-open wizard with current config pre-filled |
| `btrdasd setup --upgrade` | Regenerate files from existing config (after binary update) |
| `btrdasd setup --uninstall` | Remove all generated files, disable timers, optionally remove DB |
| `btrdasd setup --check` | Validate config + deps, report issues, change nothing |

**Root required.** The tool writes to `/etc/`, `/var/lib/`, and enables system services. It detects and refuses to run without root, printing a helpful `sudo btrdasd setup` message.

---

## System Detection

On launch, `btrdasd setup` auto-detects everything it can before asking questions, providing informed defaults.

| What | How | Purpose |
|---|---|---|
| BTRFS subvolumes | `btrfs subvolume list /` + parse mountinfo | Multi-select list of what to back up |
| Block devices | `lsblk --json -o NAME,SIZE,FSTYPE,SERIAL,MODEL,TRAN` | Identify backup targets, DAS drives |
| USB/DAS enclosures | Filter lsblk by `TRAN=usb`, check `/sys/block/*/device/model` | Recommend external drives as targets |
| ESP partitions | `lsblk` where `FSTYPE=vfat` + check for `EFI` directory | Offer ESP backup/sync and mirroring |
| Init system | Check for `systemctl` (systemd) / `/etc/init.d` (sysvinit) / `rc-service` (OpenRC) | Generate timers, cron entries, or OpenRC services |
| Package manager | Check for `pacman`/`apt`/`dnf`/`zypper`/`apk` | Dependency install commands |
| Existing deps | `which btrbk`, `which smartctl`, `which msmtp`, etc. | Report missing, offer to install |
| Existing config | Check `/etc/das-backup/config.toml` | Pre-fill for `--modify` mode |

Output: A `SystemInfo` struct passed to the wizard for defaults and validation.

---

## Interactive Wizard Flow

Each step shows detected defaults. User can accept or customize.

### Step 1: Dependencies
- Show installed/missing deps as a table (green check / red X)
- Offer to install missing: **Install all now** (single cached sudo) / **Install one at a time** (sudo per dep) / **Skip** (print commands for later)
- If cached sudo chosen, validate once with `sudo -v`, use `sudo -n` for subsequent installs

### Step 2: Select subvolumes to back up
- Multi-select from detected BTRFS subvolumes
- **All selected by default** with Select All / Deselect All options at top
- Show mount point and size for each

### Step 3: Select backup target devices
- List detected block devices (highlight USB/DAS)
- For each target: assign role (primary, mirror, ESP-sync)
- Warn if target is smaller than source

### Step 4: ESP configuration
- List detected ESP partitions
- If multiple ESPs: offer mirroring (rsync between them)
- Offer boot-event hook for auto-sync after kernel/bootloader updates (pacman hook / apt hook / dnf plugin — distro-detected)

### Step 5: Retention policy
- Defaults: 4 weekly + 2 monthly on target, 2 daily on source
- Allow override per-target

### Step 6: Scheduling
- Default: daily at 03:00, full weekly Sunday 04:00
- Generates: systemd timers / cron entries / OpenRC cron
- Custom time/frequency option

### Step 7: Email reports (optional)
- Enable/disable
- If enabled: SMTP server, port, from/to address, auth method
- Test email option

### Step 8: Install location
- Default: `/usr/local/bin` (btrdasd binary), `/usr/local/lib/das-backup/` (scripts), `/etc/das-backup/` (config), `/var/lib/das-backup/` (database)
- Allow override for each

### Step 9: GUI (optional)
- "Install KDE Plasma GUI? (requires Qt6/KF6)"
- If yes: cmake build + install
- If no: CLI-only, no Qt/KF6 dependency

### Step 10: Review and confirm
- Show full summary of all choices
- Confirm → generate config + all files + install

---

## Config Format

`/etc/das-backup/config.toml` — single source of truth.

```toml
[general]
version = "0.4.0"
install_prefix = "/usr/local"
db_path = "/var/lib/das-backup/backup-index.db"

[init]
system = "systemd"  # "systemd" | "sysvinit" | "openrc"

[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30

[[source]]
label = "nvme-root"
volume = "/.btrfs-nvme"
subvolumes = ["@", "@home", "@root", "@log"]
device = "/dev/nvme0n1p2"

[[source]]
label = "ssd-apps"
volume = "/.btrfs-ssd"
subvolumes = ["@opt", "@srv"]
device = "/dev/sdb"

[[target]]
label = "primary-22tb"
serial = "ZXA0LMAE"
mount = "/mnt/backup-22tb"
role = "primary"

[target.retention]
weekly = 4
monthly = 2

[[target]]
label = "system-2tb"
serial = "ZFL41DNY"
mount = "/mnt/backup-system"
role = "esp-sync"

[esp]
enabled = true
mirror = true
partitions = ["/dev/nvme0n1p1", "/dev/sdb1"]
mount_points = ["/efi", "/mnt/das-esp-1"]

[esp.hooks]
enabled = true
type = "pacman"  # "pacman" | "apt" | "dnf" | "none"

[email]
enabled = true
smtp_host = "127.0.0.1"
smtp_port = 1025
from = "backup@example.com"
to = "user@example.com"
auth = "plain"  # "plain" | "starttls" | "none"

[gui]
enabled = false

[dependencies]
btrbk = "/usr/bin/btrbk"
smartctl = "/usr/sbin/smartctl"
msmtp = ""
```

Key points:
- `[[source]]` and `[[target]]` are TOML arrays of tables — any number of sources/targets
- Targets identified by serial number (stable across reboots)
- ESP hooks are distro-aware
- `[gui] enabled` controls whether CMake builds the GUI
- `[dependencies]` is informational (populated by `--check`)

---

## Template Engine and Generated Files

Templates are embedded in the binary at compile time via `include_str!`. Rendered with simple `{{placeholder}}` substitution.

### Generated files

| Template | Output Location | Purpose |
|---|---|---|
| `backup-run.sh.tmpl` | `/usr/local/lib/das-backup/backup-run.sh` | Main backup script (bash) |
| `backup-verify.sh.tmpl` | `/usr/local/lib/das-backup/backup-verify.sh` | Drive verification (bash) |
| `boot-archive-cleanup.sh.tmpl` | `/usr/local/lib/das-backup/boot-archive-cleanup.sh` | Boot archive pruning (bash) |
| `btrbk.conf.tmpl` | `/etc/das-backup/btrbk.conf` | btrbk config |
| `das-backup.service.tmpl` | `/etc/systemd/system/das-backup.service` | Systemd service (if systemd) |
| `das-backup.timer.tmpl` | `/etc/systemd/system/das-backup.timer` | Systemd timer (if systemd) |
| `das-backup-full.service.tmpl` | `/etc/systemd/system/das-backup-full.service` | Full backup service |
| `das-backup-full.timer.tmpl` | `/etc/systemd/system/das-backup-full.timer` | Full backup timer |
| `das-backup-cron.tmpl` | `/etc/cron.d/das-backup` | Cron entry (sysvinit/OpenRC) |
| `esp-sync-hook.tmpl` | Distro-specific path | ESP mirror trigger |
| `das-backup-email.conf.tmpl` | `/etc/das-backup/email.conf` | SMTP config (mode 600) |

### ESP hook paths by distro
- pacman: `/etc/pacman.d/hooks/das-esp-sync.hook`
- apt: `/etc/apt/apt.conf.d/99-das-esp-sync`
- dnf: `/etc/dnf/plugins/das-esp-sync.py`

### Regeneration
`btrdasd setup --upgrade` re-reads config.toml and re-renders all templates. Safe because generated files are never hand-edited. Each generated file gets a header: `# Generated by btrdasd setup — do not edit. Modify /etc/das-backup/config.toml and run: sudo btrdasd setup --upgrade`

### Manifest
`/etc/das-backup/.manifest` records every file written during install (one path per line). Used by `--uninstall` and `--upgrade`. Written last, checked first.

### Uninstall
`btrdasd setup --uninstall` removes all files listed in `.manifest`. Idempotent — missing files skipped. Prompts before removing the database.

---

## Dependency Management

### Required
| Dep | Purpose | Packages |
|---|---|---|
| btrbk | Snapshot send/receive | `btrbk` (all distros, AUR on Arch) |
| btrfs-progs | BTRFS tools | `btrfs-progs` |
| smartmontools | Drive serial detection | `smartmontools` |
| util-linux | `lsblk`, `findmnt` | Pre-installed |

### Optional (based on config)
| Dep | When | Packages |
|---|---|---|
| msmtp / s-nail | Email enabled | `msmtp` / `s-nail` / `mailutils` |
| rsync | ESP mirroring | `rsync` |
| mbuffer | Buffered btrbk transfers | `mbuffer` |

### GUI (only if enabled)
| Dep | Packages |
|---|---|
| Qt6 | `qt6-base` / `qt6-base-dev` / `qt6-qtbase-devel` |
| KF6 | Various `kf6-*` packages |
| ECM | `extra-cmake-modules` |
| CMake | `cmake` |

### Install flow
1. Detect package manager
2. Check each dep with `which` / `command -v`
3. Display table (installed / missing)
4. Offer: Install all (cached sudo) / Install one at a time / Skip
5. Cached sudo: validate with `sudo -v`, use `sudo -n` subsequently
6. Re-check after install to confirm

---

## CMake Integration

### Options
```cmake
option(BUILD_GUI "Build KDE Plasma GUI (requires Qt6/KF6)" ON)
option(BUILD_INDEXER "Build btrdasd Rust binary" ON)
```

### Build scenarios
| Scenario | Command | Builds |
|---|---|---|
| Full (default) | `cmake -B build -DBUILD_GUI=ON && cmake --build build` | btrdasd + btrdasd-gui |
| CLI-only | `cmake -B build -DBUILD_GUI=OFF && cmake --build build` | btrdasd only |
| GUI only (dev) | `cmake -B build -DBUILD_GUI=ON -DBUILD_INDEXER=OFF` | btrdasd-gui only |

### Rust in CMake
Use `ExternalProject_Add` or `corrosion` to build `btrdasd` from `indexer/`. Output installed to `${CMAKE_INSTALL_PREFIX}/bin/`.

### GUI guard
```cmake
if(BUILD_GUI)
    add_subdirectory(gui)
endif()
```

### Docker
Multi-stage Dockerfile:
- Stage 1: Rust builder (builds `btrdasd`)
- Stage 2: Minimal runtime with `btrdasd` + bash + btrfs-progs
- No GUI in Docker (headless)
- Published to `ghcr.io/theboscoclub/das-backup-manager`

---

## License

MIT license applied to:
- `LICENSE` file in repo root
- `gui/src/main.cpp` KAboutData (`KAboutLicense::MIT`)
- Generated file headers
- Config file header comment

---

## Testing

- **Unit tests** (cargo test): TOML ser/de, template rendering, system detection parsing, config validation
- **Integration tests**: Generate config from fixture inputs, verify rendered files match expected output
- **GUI tests**: Existing 5 QTest tests unchanged
- **Runtime self-test**: `btrdasd setup --check` validates config, verifies deps, checks generated files match config

## Error Handling

- Detection failures: non-fatal (missing data means fewer defaults, user types manually)
- Template rendering failures: fatal (report which field is malformed, exit)
- Dependency install failures: report which dep failed, continue with others, summarize
- Permission errors: clear message requiring `sudo btrdasd setup`
- `--uninstall`: idempotent — missing files skipped
