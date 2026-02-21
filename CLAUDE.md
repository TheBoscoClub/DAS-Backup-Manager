# DAS-Backup-Manager

DAS backup manager: btrbk orchestration, SQLite FTS5 content indexing, KDE Plasma GUI.

## Project Rules

- **PUBLIC REPO** — TheBoscoClub/DAS-Backup-Manager on GitHub. Push allowed.
- **C++20** — Qt6 6.10.2, KF6 6.23.0, CMake 4.2.3, SQLite 3.51.2 FTS5
- **Scripts use zsh** — All scripts `#!/usr/bin/env zsh`
- **systemd-boot** — CachyOS uses systemd-boot, NOT grub
- **BTRFS RAID-1** — Backup targets on HDD RAID-1 and DAS enclosure

## Key Paths

- **Backup DB**: `/var/lib/das-backup/backup-index.db`
- **btrbk config**: `/etc/btrbk/btrbk.conf`
- **Email config**: `/etc/das-backup-email.conf`
- **Growth log**: `/var/lib/das-backup/growth.log`

## Build

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
```

## Detailed Rules

See `.claude/rules/` for project-specific rules:
- `build.md` — CMake, Qt6/KF6, C++20 build conventions
- `backup.md` — btrbk, DAS, retention, boot archival
