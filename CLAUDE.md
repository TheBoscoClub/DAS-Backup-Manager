# DAS-Backup-Manager

DAS backup manager: btrbk orchestration, SQLite FTS5 content indexing, KDE Plasma GUI.

## Project Rules

- **PUBLIC REPO** — TheBoscoClub/DAS-Backup-Manager on GitHub. Push allowed.
- **Rust** — Library (`buttered_dasd`) + CLI (`btrdasd`): Rust 2024 edition, rusqlite 0.40 (bundled FTS5), clap 4.6, walkdir 2.5
- **C++20** — GUI (`btrdasd-gui`): Qt6 6.11.2, KF6 6.30.0, CMake 4.4.4 (installed; minimums Qt 6.6, KF 6.0, CMake 3.25)
- **BTRFS RAID-1** — Backup targets on HDD RAID-1 and DAS enclosure

## Key Paths

- **Backup DB**: `/var/lib/das-backup/backup-index.db`
- **Config**: `/etc/das-backup/config.toml` (source of truth); `/etc/btrbk/btrbk.conf` is generated from it — never hand-edit
- **Email**: local relay `127.0.0.1:25`, unauthenticated; this project stores **no** mail credential — `.claude/rules/backup.md` §Email Reports
- **Growth log**: `/var/lib/das-backup/growth.log`

## Build

```bash
# Rust is built by CMake into build/cargo-target/; never a bare `cargo build`
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
# Rust tests (target dir on tmpfs, not the array)
cd indexer && CARGO_TARGET_DIR=/tmp/das-backup-target cargo test --features dbus
```

## Detailed Rules (`.claude/rules/`)

- `esp-safety.md` — **CRITICAL** — DAS ESP partition safety (never sync host ESP onto DAS drives)
- `build.md` — CMake, Qt6/KF6, C++20, Rust build conventions
- `backup.md` — btrbk, DAS, retention, boot archival, email, exit codes, locks
- `fail-silent.md` — which error suppressions are legitimate here, and which are always defects
