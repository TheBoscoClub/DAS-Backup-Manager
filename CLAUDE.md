# DAS-Backup-Manager

DAS backup manager: btrbk orchestration, SQLite FTS5 content indexing, KDE Plasma GUI.

## Project Rules

- **PUBLIC, GENERIC PROJECT** — no installation-specific values (serials, UUIDs, hostnames, paths, addresses) in tracked files; use placeholders (`SERIALnn`, `example.org`).
- **Rust** — Library (`buttered_dasd`) + CLI (`btrdasd`): Rust 2024 edition, rusqlite (bundled FTS5), clap, walkdir
- **C++20** — GUI (`btrdasd-gui`): Qt6, KF6 (minimums Qt 6.6, KF 6.0, CMake 3.25)
- **BTRFS** — backup targets on a DAS enclosure; RAID-1 primary, independent recovery drives

## Key Paths

- **Backup DB**: `/var/lib/das-backup/backup-index.db`
- **Config**: `/etc/das-backup/config.toml` (source of truth); `/etc/btrbk/btrbk.conf` is generated from it — never hand-edit
- **Email**: local relay `127.0.0.1:25`, unauthenticated; this project stores **no** mail credential — `.claude/rules/backup.md` §Email Reports
- **Growth log**: `/var/lib/das-backup/growth.log`

## Build

```bash
# Rust is built by CMake into build/cargo-target/; never a bare `cargo build`
# Install prefix defaults to /usr (not CMake's /usr/local); packaging passes it explicitly
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
# Rust tests (build output on fast scratch, not the project disk)
cd indexer && CARGO_TARGET_DIR=/tmp/das-backup-target cargo test --features dbus
```

## Detailed Rules

- `esp-safety.md` — **CRITICAL** — recovery-drive ESP identification, independence and incident
- `build.md` — CMake, Qt6/KF6, C++20, Rust build conventions
- `backup.md` — btrbk, DAS, boot archival, email, exit codes, locks, supervisor interaction
- `fail-silent.md` — which error suppressions are legitimate here, and which are always defects
- `test-safety.md` — a suite must never take the host down: task cap, no self-calling stubs
- `tracking.md` — where a new bd issue goes (phase epics) and the orphan check
- `local-host-identifiers.md` — (gitignored, this machine only) real serials/UUIDs behind the tracked placeholders
- `local-installation.md` — (gitignored, this machine only) this installation's targets, enclosure, ESPs, mail, supervisor notes
