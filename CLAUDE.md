# DAS-Backup-Manager

DAS backup manager: btrbk orchestration, SQLite FTS5 content indexing, KDE Plasma GUI.

## Project Rules

- **PUBLIC REPO** — TheBoscoClub/DAS-Backup-Manager on GitHub. Push allowed.
- **Rust** — Library (`buttered_dasd`) + CLI (`btrdasd`): Rust 2024 edition, rusqlite 0.40 (bundled FTS5), clap 4.6, walkdir 2.5
- **C++20** — GUI (`btrdasd-gui`): Qt6 6.10.2, KF6 6.23.0, CMake 4.2.3
- **BTRFS RAID-1** — Backup targets on HDD RAID-1 and DAS enclosure

## Key Paths

- **Backup DB**: `/var/lib/das-backup/backup-index.db`
- **btrbk config**: `/etc/btrbk/btrbk.conf`
- **Email transport**: local mail relay at `127.0.0.1:25`, unauthenticated. This project stores **no** mail credential — see `.claude/rules/backup.md` §Email Reports
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
- `esp-safety.md` — **CRITICAL** — DAS ESP partition safety (never sync host ESP onto DAS drives)
- `build.md` — CMake, Qt6/KF6, C++20 build conventions
- `backup.md` — btrbk, DAS, retention, boot archival


