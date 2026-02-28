# DAS-Backup-Manager

DAS backup manager: btrbk orchestration, SQLite FTS5 content indexing, KDE Plasma GUI.

## Project Rules

- **PUBLIC REPO** — TheBoscoClub/DAS-Backup-Manager on GitHub. Push allowed.
- **Rust** — Library (`buttered_dasd`) + CLI (`btrdasd`): Rust 2024 edition, rusqlite 0.38 (bundled FTS5), clap 4.5, walkdir 2.5
- **C++20** — Future GUI: Qt6 6.10.2, KF6 6.23.0, CMake 4.2.3
- **Scripts use bash** — All scripts `#!/bin/bash`
- **systemd-boot** — CachyOS uses systemd-boot, NOT grub
- **BTRFS RAID-1** — Backup targets on HDD RAID-1 and DAS enclosure

## Key Paths

- **Backup DB**: `/var/lib/das-backup/backup-index.db`
- **btrbk config**: `/etc/btrbk/btrbk.conf`
- **Email config**: `/etc/das-backup-email.conf`
- **Growth log**: `/var/lib/das-backup/growth.log`

## Build

```bash
# Indexer (Rust)
cd indexer && cargo build --release && cargo test

# Scripts/systemd (CMake)
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
```

## Detailed Rules

See `.claude/rules/` for project-specific rules:
- `build.md` — CMake, Qt6/KF6, C++20 build conventions
- `backup.md` — btrbk, DAS, retention, boot archival
