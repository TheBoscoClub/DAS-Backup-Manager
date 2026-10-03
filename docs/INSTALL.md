# DAS-Backup-Manager — Installation Guide

**Version**: 0.7.22.3

## Before You Begin

### Minimum Requirements

- Linux with BTRFS support (kernel 5.15+)
- DAS enclosure (any manufacturer, any interface -- USB, Thunderbolt, eSATA) in JBOD mode
- One or more BTRFS-formatted drives (any technology: HDD, SSD, NVMe)
- btrbk 0.32+, smartmontools
- Rust 1.88+ with Cargo (needs let-chains; not compile-tested below 1.98.1) for building btrdasd

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
| Rust toolchain | **1.88+** | Edition 2024; needs let-chains (stable since 1.88); not compile-tested below 1.98.1 |
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
| Qt6 | 6.6+ (tested 6.11.2) | UI framework |
| Qt6 Charts | 6.6+ (tested 6.11.2) | Growth trendline chart (`qt6-charts` package) |
| KDE Frameworks 6 | 6.0+ (tested 6.30.0) | KXmlGuiWindow, KIO, KAboutData, Notifications, StatusNotifierItem |
| CMake | 3.25+ (tested 4.4.3) | Build system for GUI component |
| Extra CMake Modules (ECM) | ships with KF6 | KDE-specific CMake macros |

## Quick Start — Full Build (CLI + GUI + Helper)

The recommended installation method builds all components and runs the setup wizard:

```bash
# 1. Build everything (CLI, D-Bus helper, KDE GUI)
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build

# 2. Install all components (binaries, scripts, systemd, D-Bus, polkit, man page, icons)
sudo cmake --install build

# 3. Run the interactive setup wizard
sudo btrdasd setup
```

This installs: `btrdasd` (CLI), `btrdasd-gui` (KDE GUI), `btrdasd-helper` (D-Bus daemon), backup scripts, systemd units, D-Bus/polkit configs, shell completions, man page, and desktop entry.

The wizard auto-detects the init system, package manager, and installed dependencies
before it starts, then walks through the following on-screen steps (numbered `[1/9]`
through `[9/9]`; the ESP-mirroring step that once sat between targets and retention was
removed 2026-04-12 along with all ESP sync code):

1. **Checking Dependencies** `[1/9]` — verifies btrbk, btrfs, smartctl, etc. against the
   auto-detected system info
2. **Backup Sources (BTRFS Subvolumes)** `[2/9]` — choose BTRFS subvolumes to back up
3. **Backup Targets** `[3/9]` — choose backup destination drives
4. **Retention Policy** `[4/9]` — weekly and monthly snapshot counts per target
5. **Backup Schedule** `[5/9]` — incremental and full backup times
6. **Email Notifications** `[6/9]` — optional; relay host/port and the
   from/to addresses (no credentials are requested or stored)
7. **Install Location** `[7/9]` — binary/script install prefix
8. **KDE Plasma GUI** `[8/9]` — GUI desktop entry install toggle
9. **Review Configuration** `[9/9]` — shows generated config, writes files

## Installer Modes

### Fresh Install (default)

```bash
sudo btrdasd setup
```

Runs the full 9-step wizard, generates all configuration files, and enables backup timers.

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

No credential file is touched, because none exists: mail submission is
unauthenticated and the relay's upstream key is the relay's own business.

Prompts whether to also remove the backup database at `/var/lib/das-backup/backup-index.db`. The TOML config file is preserved for potential reinstallation.

### Full Uninstall (everything)

```bash
sudo btrdasd setup --uninstall-all
```

Removes all generated files (same as `--uninstall`), then also removes cmake-installed components: binaries (`btrdasd`, `btrdasd-gui`, `btrdasd-helper`), D-Bus configs, polkit policy, systemd units, man page, shell completions, desktop entry, and icon. Prompts whether to remove the backup database.

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

# Configure btrbk manually. Note: once a config.toml exists, btrbk.conf is
# generated from it and a hand edit is lost the next time sync or
# `btrdasd setup --upgrade` runs (see [subvolumes] below).
sudo cp config/btrbk.conf /etc/btrbk/btrbk.conf
sudo vim /etc/btrbk/btrbk.conf  # edit for your drives

# Email needs no credentials — reports are submitted unauthenticated to a
# local mail relay ([email].smtp_host/smtp_port, default 127.0.0.1:25).
# Verify one is listening:  ss -ltn | grep ':25 '

# Enable systemd timers
sudo systemctl enable --now das-backup.timer das-backup-full.timer
```

## CLI-Only Build (no GUI dependencies)

If you don't have Qt6/KF6 installed or don't need the GUI:

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_GUI=OFF
cmake --build build
sudo cmake --install build
```

This still installs the CLI, D-Bus helper, backup scripts, systemd units, polkit policy, and man page — everything except the GUI.

## CMake Build Options

| Option | Default | Description |
|--------|---------|-------------|
| `BUILD_GUI` | `ON` | Build the KDE Plasma GUI (requires Qt6/KF6) |
| `BUILD_INDEXER` | `ON` | Build the `btrdasd` Rust binary via cargo |
| `BUILD_HELPER` | `ON` | Build the `btrdasd-helper` D-Bus daemon and install polkit/D-Bus config |
| `CMAKE_INSTALL_PREFIX` | `/usr/local` | Installation prefix for binaries and scripts |
| `CMAKE_BUILD_TYPE` | (unset) | `Release`, `RelWithDebInfo`, or `Debug` |

### CLI-Only Build (no GUI dependencies)

```bash
cmake -B build -DBUILD_GUI=OFF -DCMAKE_BUILD_TYPE=Release
cmake --build build
```

This skips Qt6/KF6 entirely — no GUI libraries needed on the system.

### Indexer-Only Build (CLI without the helper)

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_GUI=OFF -DBUILD_HELPER=OFF
cmake --build build
# Binary at: build/cargo-target/release/btrdasd
```

Build through CMake rather than a bare `cargo build`: CMake passes `--target-dir build/cargo-target/`, which is where the install step looks for the binaries.

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

**Minimum Rust version**: 1.88 (needs let-chains in edition 2024; not compile-tested below 1.98.1, and `Cargo.toml` declares no `rust-version`). Distributions shipping older Rust (e.g., Debian 13 with 1.85) require [rustup](https://rustup.rs/) for compilation.


## Configuration Reference

The installer generates `/etc/das-backup/config.toml` with the following sections:

### `[general]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `version` | string | the running build's `CARGO_PKG_VERSION` (3-part semver, e.g. `"0.7.22"`) | Config format version |
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

### `[das]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `model_pattern` | string | `"TDAS"` | Drive model (spaces removed) that `backup-verify.sh` matches to identify DAS drives |
| `io_scheduler` | string | `"mq-deadline"` | I/O scheduler `backup-run.sh` sets on the DAS drives during a run |
| `mount_opts` | string | `""` (none) | Mount options for every backup-target mount, by `backup-run.sh` and by `btrdasd` (`MountGuard`) alike |

### `[[source]]` (array)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `label` | string | — | Human-readable name (e.g., `"nvme-root"`) |
| `volume` | string | — | BTRFS volume mount point (e.g., `"/.btrfs-nvme"`) |
| `device` | string | — | Block device path (e.g., `"/dev/nvme0n1p2"`) |
| `snapshot_dir` | string | `".btrbk-snapshots"` | Directory on the source volume that holds btrbk's local snapshots |
| `target_subdirs` | string[] | `[]` | Subdirectory on each target this source sends into |
| `target_labels` | string[] | `[]` (all targets) | `[[target]].label` values this source sends to; bulk data should name only the primary |

Subvolumes are an array of tables, `[[source.subvolumes]]`, one per subvolume (a bare string such as `"@"` is also accepted):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | — | Subvolume path on the volume (e.g., `"@home"`) |
| `manual_only` | bool | `false` | Flag set by `btrdasd subvol set-manual` / `set-auto` and inherited by nested adoptions. Not consulted by `btrbk.conf` rendering or `backup-run.sh`, so it does not keep a subvolume out of scheduled backups; `btrdasd backup run` (with no `--sources`) skips a source only when every one of its subvolumes is manual-only (tracked as bd `9ry`) |
| `snapshot_name` | string | derived | Overrides the snapshot name btrbk would derive (adopted entries always carry one) |
| `adopted`, `retired` | date | absent | Written by the backup run; see `[subvolumes]` below |

### `[[target]]` (array)

| Field | Type | Description |
|-------|------|-------------|
| `label` | string | Human-readable name (e.g., `"primary-22tb"`) |
| `serials` | string[] | Expected drive serials: one for a single drive, two for a RAID-1 pair. A legacy single `serial = "…"` is still read. Advisory — a missing serial warns, it does not abort |
| `mount_uuid` | string | BTRFS filesystem UUID. When set, the target is found and mounted by UUID on both the script and the CLI/GUI path, tolerating the loss of one RAID-1 member; `btrdasd setup --check` reports targets that lack it. A target needs at least one of `serial`, `serials`, `mount_uuid` |
| `mount` | string | Mount point (e.g., `"/mnt/backup-22tb"`) |
| `role` | enum | `"primary"` or `"mirror"` |
| `display_name` | string | Optional human-readable name for the target (e.g., `"22TB Exos RAID-1 (Bays 2+5)"`) |
| `retention.daily` | u32 | Number of daily snapshots to retain |
| `retention.weekly` | u32 | Number of weekly snapshots to retain |
| `retention.monthly` | u32 | Number of monthly snapshots to retain |
| `retention.yearly` | u32 | Number of yearly snapshots to retain |

Each retention count defaults to `0` when omitted. Targets are mounted by `backup-run.sh` or `btrdasd` for the duration of an operation and unmounted afterwards (a busy unmount is retried 5 times, 2 seconds apart; a target that is still mounted at the end fails the job with `still mounted: <path>`) — never by `/etc/fstab`.

### `[boot]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable boot subvolume archival (archive-then-recreate on `--full` runs) |
| `subvolumes` | string[] | `["@", "@home"]` | Subvolumes archived and recreated |
| `archive_retention_days` | u32 | `60` | Days to retain `@.archive.*`/`@home.archive.*` snapshots before `boot-archive-cleanup.sh` prunes them |

Snapshot names are read from `/etc/btrbk/btrbk.conf`, not derived from this section — if that file cannot be read, the boot-archive step declines rather than guessing. The replacement snapshot is located and built alongside the live subvolume **before** the live one is removed, so no failure path can leave `@` absent (`bd DAS-Backup-Manager-5ig`).

### `[restore]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `allowed_roots` | string[] | `["/home", "/tmp"]` | Roots a restore may write beneath. A destination must resolve to a path under one of these |

Restores run as root under `btrdasd-helper`, so the destination is policy rather than a free parameter. Two rules apply on top of `allowed_roots`, and neither is configurable:

- **A built-in denylist always wins.** `/etc`, `/usr`, `/boot`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/root`, `/var/lib`, `/var/spool`, `/dev`, `/proc`, `/sys`, `/srv/http` and `/srv/ftp` are refused even if you list one in `allowed_roots` — the denylist is checked first. These are the paths where a restored file becomes executable code or changes system identity; the two `/srv` entries are the distro-default document roots, so a file restored into either is *served* to whoever can reach the listener. Comparison is component-wise, so a sibling such as `/srv/http-archive` is unaffected.
- **Writes use `O_NOFOLLOW`**, so a symlink pre-planted at the destination fails the open rather than being followed.

Both roots are compared after the path is resolved, so a symlinked ancestor cannot get around them. To restore somewhere else — an external drive, a staging area under `/mnt` — add that root here rather than working around the check.

Grant a **subdirectory**, never a parent that also holds served or system content. This host adds `/srv/VirtualMachines` so the `ssd-vm` source can be restored in place — backing up something that cannot be restored is half a mechanism — and deliberately not `/srv`, which also contains the document roots denied above.

### `[subvolumes]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `exclude` | string[] | `[]` | Glob patterns (`*`, `?`; `*` also matches `/`) for subvolumes the backup run must not adopt, on top of the built-in `@tmp` and `@var-tmp` |

Every backup run adopts each subvolume that exists on a source volume and is not excluded (`btrdasd subvol sync`, run before btrbk), so creating a subvolume is enough to have it backed up. A pattern names the subvolume **without a trailing slash** and also covers everything nested under it: `@cache` excludes `@cache` and `@cache/anything`, while `@cache/` matches nothing. Anything under a `.snapshots` or `.btrbk-snapshots` directory is always skipped. A subvolume that has its own `[[source.subvolumes]]` entry is never skipped by a pattern. Every adoption and every exclusion, with the pattern that caused it, is listed in the run report (snapshot-tree skips as one count per volume); `sudo btrdasd subvol sync --dry-run` prints the same plan and changes nothing, and `backup-run.sh --dryrun` runs the sync step that way. A dry run (`backup-run.sh --dryrun`, `btrdasd backup run --dry-run`) changes no configuration and sends nothing; the one thing it creates is a missing empty target directory — for instance the one a pending adoption's new `<source>-adopted` source will receive into — and it logs each one. `btrdasd backup run` and a backup started from the GUI run the same sync before btrbk; a sync failure does not stop that run, which is recorded as failed (report and history) and, from the CLI, exits non-zero at the end. Neither of them runs `subvol expire` — only `backup-run.sh` does.

`/etc/btrbk/btrbk.conf` is generated from `config.toml`. A sync that changes `config.toml` writes both files together, even if some volume could not be read. A sync with nothing to change rewrites `btrbk.conf` whenever it differs from what `config.toml` renders to, and the report says so, except on a dry run or when the sync failed (a volume was not read) — then the file is left alone and the report says it is out of date; `btrdasd subvol add` / `remove` / `set-manual` / `set-auto`, every configuration change made from the GUI, and `btrdasd setup --upgrade` rewrite it too. A hand edit is lost the next time any of them does — change `config.toml` instead.

Two optional dates on a `[[source.subvolumes]]` entry are written by the run and never need editing by hand:

| Field | Description |
|-------|-------------|
| `adopted` | `YYYY-MM-DD` (UTC) the run added the entry. Absent on hand-written entries |
| `retired` | `YYYY-MM-DD` (UTC) the subvolume was found gone. A retired entry is left out of `btrbk.conf`; `btrdasd subvol expire` deletes its snapshots once the retirement date plus the retention window has passed — per target the longest window of that target, on the source side the shortest of the targets' windows; a target with no retention keeps them and says so — then removes the entry. The field is cleared if the subvolume comes back |

A subvolume nested under a configured one joins that ancestor's source, so it takes the ancestor's targets, snapshot directory, target subdirectory and `manual_only` flag. One with no configured ancestor goes through the volume's `<first-source-label>-adopted` source, created on first need: it takes `device` and `snapshot_dir` from the first source on that volume, uses its own label as the target subdirectory, and sends to the primary target only (edit `target_labels` there if more targets are wanted). Each adopted entry gets an explicit `snapshot_name`: the name btrbk would have derived, made unique across the whole config with `-2`, `-3`, … Existing names never change.

`btrdasd subvol expire` looks for a retired entry's snapshots in each *location*: every target directory its source sends to, and the source's own snapshot directory. On a target they go once the retirement date plus that target's longest retention window has passed; on the source side, once the retirement date plus the **shortest** of the targets' windows has passed.

A wrong date mostly costs a late deletion. Nothing of a retired entry is deleted while its source volume is not mounted (nobody can tell whether the subvolume is back), nor while a path with its name exists on its mounted source volume (sync will revive the entry), nor at a location whose newest snapshot of the series is dated more than a day after the retirement date (the clock was wrong at retirement, or the date was edited — correct `retired`). If the system clock reads before 2026-01-01, sync stamps no retirement and expiry deletes nothing; both report the clock and fail. A clock that wrongly reads far in the future at expiry time is not detected and would expire retired series early (live subvolumes are never touched by expiry). When a run's sync fails, `backup-run.sh` runs expiry as a dry run only.

### `[doctor]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `exclude` | string[] | `[]` | Still read, and merged with `[subvolumes].exclude`, so existing configs keep working. Prefer `[subvolumes].exclude` — it governs adoption as well as `btrdasd doctor --check-drift` |

Because the backup run adopts new subvolumes itself, a **missing** or **stale** finding from the drift check now means the sync step failed or has not run since the subvolume appeared; the report advises `sudo btrdasd subvol sync --dry-run`.

**Exit codes**, because systemd cannot tell a finding from a malfunction:

| Code | Meaning | `das-backup-doctor.service` |
|------|---------|------------------------------|
| `0` | Ran clean, or deferred because a backup/scrub holds the maintenance lock | success |
| `1` | **Drift found** — missing or stale subvolumes. A successful check *with a result* | **success**, via `SuccessExitStatus=1` |
| `3` | At least one volume failed to mount/list while others were checked — those subvolumes went unexamined | **failed** |
| `2` | Could not run at all — config load failure, lock I/O error, or every volume failed | **failed** |

`3` outranks `1` when both occur: an incomplete check cannot assert that its drift list is complete. Without
`SuccessExitStatus=1` systemd marked the unit `failed` on a mere finding, and cachyos-sentinel then restarted it and
notified that the backup checker had failed — at the exact moment it had worked and had something to say. The finding
itself travels by email (`--email`) and the journal report, not by the exit code.

**Removed section**: `[esp]` (enabled, mirror, partitions, mount_points, hooks.enabled,
hooks.type) — ESP/boot partition mirroring was removed from the codebase on 2026-04-10
(orphan pacman hook generator) and 2026-04-12 (remaining `Esp` struct and `sync_esp()`);
see `.claude/rules/esp-safety.md`. Old `config.toml` files with a leftover `[esp]` section
are silently ignored by serde on load.

### `[email]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable email backup reports |
| `smtp_host` | string | `"127.0.0.1"` | Local mail relay host |
| `smtp_port` | u16 | `25` | Local mail relay port |
| `from` | string | `"backup@localhost"` | Sender address — **also the SMTP envelope sender**, which is how a sender-dependent relay picks its upstream credential |
| `to` | string | `"root@localhost"` | Recipient address |

Submission is **unauthenticated plaintext** — btrdasd stores no mail credential
and never asks for one. Point `smtp_host`/`smtp_port` at a local relay (Postfix,
msmtp, or any SMTP listener) and let it own authentication, TLS, and retries.
`DAS_REPORT_FROM` / `DAS_REPORT_TO` override `from`/`to` at run time for testing.

Two behaviour changes landed with the 2026-08-06 relay migration:

- **These keys are now read.** Before, both senders parsed Protonmail Bridge
  credentials and ignored `[email]` entirely, so editing this table changed
  nothing. `enabled = false` was likewise ignored by the shell path.
- **`auth` was removed.** Unauthenticated submission has no auth method to
  choose. An `auth = …` line in an existing config is ignored, not an error.
  `btrdasd setup --upgrade` rewrites the Bridge port `1025` to `25`; any other
  port is left alone, and `btrdasd setup --check` reports whether the relay is
  actually listening.

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

### `[recovery_os]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_age_days` | u32 | `60` | Days since a recovery OS's last full system upgrade before it is reported `STALE` |

Every `role = "mirror"` target is taken to carry its own bootable install under `@`. After btrbk, while the targets are mounted, `backup-run.sh` runs `btrdasd recovery-os status`, which reads that install — never writes to it — and adds a `RECOVERY OS` section to the report: OS name, last full upgrade and its age in days, newest kernel against the host's running kernel, btrfs-progs against the host's, btrbk and das-backup-manager. A recovery OS is also `STALE`, whatever this setting, when its newest kernel's major.minor series is behind the host's, or when its upgrade date or kernel cannot be read — unknown is never shown as current. A stale one marks the operation `WARN`: the report status reads `COMPLETED WITH WARNINGS` and the subject `SUCCESS WITH WARNINGS`, while the run itself is still recorded as a success. The reading is kept in `/var/lib/das-backup/recovery-os.json`, so `btrdasd health` shows it between runs. Updating a recovery OS: `docs/DISASTER-RECOVERY-GUIDE.md`, "Keeping the recovery OSes current".

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
| `/etc/udev/rules.d/99-das-backup-udisks-ignore.rules` | udev rule hiding every backup target from udisks2, so no desktop session automounts it — one line per `[[target]]` serial, plus one per `mount_uuid`. Generated on every init system; `btrdasd setup --check` reads back whether each attached target actually carries the flag |

`${prefix}` is `[general].install_prefix` (default `/usr/local`, `/usr` on the live system
here). There is no generated credential file and no credential of any kind —
mail submission is unauthenticated to a local relay. ESP pacman hook generation
was removed 2026-04-10; no ESP hook file is ever generated.

The backup and scrub services are ordered after `time-sync.target` (retirement and expiry dates depend on a correct clock). For sysvinit/OpenRC systems, cron entries replace systemd units.

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
