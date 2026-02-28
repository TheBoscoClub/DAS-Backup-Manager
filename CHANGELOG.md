# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`buttered_dasd` library crate** — Extracted 11 public modules from CLI binary into reusable library (`backup`, `config`, `db`, `health`, `indexer`, `progress`, `report`, `restore`, `scanner`, `schedule`, `subvol`)
- **`SubvolConfig` data model** — Replaced `Vec<String>` subvolumes with `Vec<SubvolConfig>` supporting `manual_only` flag (backward-compatible `#[serde(untagged)]` deserialization)
- **New CLI subcommands** — `backup` (run/snapshot/send/boot-archive/report), `restore` (file/snapshot/browse), `schedule` (show/set/enable/disable/next), `subvol` (list/add/remove/set-manual/set-auto), `health`, `config edit`
- **`NewBackupRun` struct** — Structured input for backup run recording (replaces positional parameters)
- **Database tables** — `backup_runs` and `target_usage` tables for backup history and disk usage tracking

### Changed
- **Crate architecture** — Split from CLI-only binary into library (`buttered_dasd`) + binary (`btrdasd`) with `[lib]` and `[[bin]]` sections in Cargo.toml
- **Regex performance** — `LazyLock<Regex>` for compile-once snapshot dirname parsing (replaces per-call `Regex::new()`)
- **Release profile** — Added `[profile.release]` with `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = true`

### Fixed

## [0.5.1] - 2026-02-24

### Added
- **Full management interface design** — Architecture for transforming GUI from read-only browser into full backup management system with CLI parity
- **Design document** (`docs/plans/2026-02-24-full-management-interface-design.md`) — Complete architecture spec for v0.6.0
- **Implementation plan** (`docs/plans/2026-02-24-full-management-implementation-plan.md`) — 41-task phased plan across 5 phases

### Planned for v0.6.0
- **D-Bus privileged helper** (`btrdasd-helper`) — polkit-authorized daemon for privileged operations (backup, restore, config write, schedule modify, SMART queries)
- **GUI expansion** — Navigation sidebar, Dolphin-style snapshot file browser, backup operations panel, comprehensive config editor, first-run wizard, progress panel with structured progress + raw log, health monitoring dashboard, backup history view
- **Comprehensive documentation** — Full man page, GNU info page, HTML docs, rich `--help` with examples, shell completions (bash/zsh/fish)
- **Desktop integration** — KNotification, optional system tray, keyboard shortcuts
- **`--json` flag** — Machine-readable JSON output on all read commands

## [0.5.0] - 2026-02-22

### Added
- **Config-driven pipeline** (`btrdasd config dump-env`) — Reads `config.toml` and prints shell-sourceable `DAS_*` key=value pairs; scripts source config at runtime via `eval`
- **Config subcommands** — `btrdasd config dump-env`, `btrdasd config show`, `btrdasd config validate`
- **Extended config.toml schema** — New `[das]`, `[boot]` sections; per-source `snapshot_dir`; per-target `display_name`, `retention.daily`, `retention.yearly`
- **Hardware-agnostic documentation** — All docs describe the system generically; author's hardware moved to `docs/examples/` as reference examples
- **Planning worksheet** — Capacity estimation, drive selection, retention planning guide in `docs/OFFLINE-BACKUP-PLAN.md`
- **Generic bay mapping guide** — LED identification, serial mapping, config.toml integration in `docs/DAS-BAY-MAPPING.md`
- **Reference examples directory** — `docs/examples/` with author's bay mapping, storage topology, and index

### Changed
- **Scripts refactored** — `backup-run.sh`, `backup-verify.sh`, `boot-archive-cleanup.sh`, `das-partition-drives.sh` now use `eval "$(btrdasd config dump-env)"` instead of hardcoded values
- **Template engine** — Generated backup script replaced with thin `exec` wrapper; production scripts embedded via `include_str!` and copied during install
- **systemd units** — Use production paths (`/usr/local/lib/das-backup/`) and generic DAS detection instead of hardcoded dev paths
- **Documentation** — `STORAGE-ARCHITECTURE-AND-RECOVERY.md`, `DISASTER-RECOVERY-GUIDE.md`, `DAS-BAY-MAPPING.md`, `OFFLINE-BACKUP-PLAN.md` all parameterized with `<your-uuid>` placeholders

### Fixed
- **GUI restore action** — Implemented `Database::snapshotPathById()` and `m_currentSnapshotId` tracking; restore now correctly combines snapshot path with file path for `KIO::copy`

## [0.4.0] - 2026-02-21

### Added
- **KDE Plasma GUI** (`btrdasd-gui`) — Native Qt6/KF6 application for browsing and restoring backup files
  - 12 C++ components: MainWindow, Database, SnapshotModel, FileModel, SearchModel, SnapshotTimeline, IndexRunner, SnapshotWatcher, RestoreAction, SettingsDialog, desktop entry, XML GUI
  - Custom-painted timeline widget for visual snapshot navigation
  - FTS5 full-text search with debounced input
  - KIO-based file restore with destination chooser
  - QFileSystemWatcher auto-detection of new snapshots
  - KConfigDialog settings with database path, watch path, auto-watch toggle
  - 4 QTest suites (database, snapshotmodel, filemodel, searchmodel)
- **Interactive installer** (`btrdasd setup`) — 10-step dialoguer wizard with 5 modes:
  - `btrdasd setup` — Fresh install with interactive configuration
  - `btrdasd setup --modify` — Re-open wizard with existing config pre-filled
  - `btrdasd setup --upgrade` — Regenerate files from existing config after binary update
  - `btrdasd setup --uninstall` — Remove all generated files, optionally remove database
  - `btrdasd setup --check` — Validate config, verify files, check dependencies
  - System detection: block devices, BTRFS subvolumes, init system (systemd/sysvinit/OpenRC), package manager
  - Template engine: generates btrbk.conf, systemd/cron units, backup script, email config, ESP hooks
  - TOML-based configuration at `/etc/das-backup/config.toml`
- **Dockerfile** — Multi-stage build (rust:1.93-bookworm builder + debian:bookworm-slim runtime) for headless `btrdasd` CLI
- **CMake build options** — `BUILD_GUI` and `BUILD_INDEXER` toggles; `ExternalProject_Add` for Rust cargo build
- **Distro-agnostic init system support** — systemd, sysvinit, and OpenRC service/timer generation
- **docs/ARCHITECTURE.md** — Full system architecture with security and design decisions
- **docs/INSTALL.md** — Comprehensive installation guide for all 5 installer modes

### Changed
- **License**: GPL-3.0 → MIT
- CMake project version: 0.1.0 → 0.4.0
- systemd units now generated by installer from templates (no longer static files in `systemd/` directory)
- Rust minimum version: 1.85 → 1.87+ (edition 2024 `let_chains` feature)
- Indexer (`buttered-dasd` crate) version: 0.1.0 → 0.4.0
- GUI (`btrdasd-gui`) version: 0.1.0 → 0.4.0

### Fixed
- systemctl calls moved from `install_to_prefix` to `install` to prevent polkit authentication dialogs during test runs

## [0.3.0] - 2026-02-21

### Added
- **ButteredDASD content indexer** (`btrdasd`) — Rust CLI for indexing DAS backup snapshots
  - SQLite FTS5 full-text search across all indexed file paths and names
  - Span-based deduplication: unchanged files across consecutive snapshots stored as single row
  - Incremental indexing: only walks newly-created snapshots
  - 4 CLI subcommands: `walk` (index), `search` (FTS5), `list` (snapshot contents), `info` (stats)
  - WAL journal mode for concurrent read/write
  - Performance indexes on snapshots, files, and spans tables
  - 37 unit tests, zero clippy warnings, cargo audit clean
- Integrated `btrdasd` into `scripts/backup-run.sh` with soft-fail (indexing errors don't abort backup)
- Content indexer status line in email backup reports

### Changed
- Indexer built in Rust (edition 2024) instead of planned C++ for memory safety
- Application named ButteredDASD with CLI binary `btrdasd`
- Indexer binary path in backup-run.sh uses `BTRDASD_BIN` env var with `/usr/local/bin/btrdasd` default

## [0.2.0] - 2026-02-21

### Added
- Migrated backup scripts from CachyOS-Kernel project
  - `scripts/backup-run.sh` v3.1.0 — btrbk orchestrator with triple-target architecture, throughput logging, email reports
  - `scripts/backup-verify.sh` v2.0.0 — DAS drive health (SMART) + btrbk status verification
  - `scripts/das-partition-drives.sh` v1.0.0 — DAS drive partitioning with serial verification
  - `scripts/install-backup-timer.sh` — systemd timer installer (updated for new project structure)
  - `scripts/boot-archive-cleanup.sh` v1.0.0 — NEW: prune boot subvolume archives older than retention period
- Migrated btrbk reference config to `config/btrbk.conf`
- Created `config/das-backup-email.conf.example` — email config template (redacted credentials)
- Migrated systemd units to `systemd/` (paths updated for DAS-Backup-Manager)
  - `das-backup.service` + `das-backup.timer` — nightly incremental at 03:00
  - `das-backup-full.service` + `das-backup-full.timer` — weekly full on Sundays at 04:00
- Migrated documentation to `docs/`
  - `OFFLINE-BACKUP-PLAN.md` — capacity planning, drive allocation, backup strategy
  - `DISASTER-RECOVERY-GUIDE.md` — step-by-step recovery procedures
  - `STORAGE-ARCHITECTURE-AND-RECOVERY.md` — full system storage reference
  - `DAS-BAY-MAPPING.md` — physical drive locations and serial numbers
- CMakeLists.txt with install targets for scripts, config, and systemd units

## [0.1.0] - 2026-02-21

### Added
- Project scaffolding with CMake build system (ECM + Qt6 + KF6)
- GitHub repo with full security: Dependabot, CodeQL, secret scanning, branch protection
- GPL-3.0 license (changed to MIT in v0.4.0)

[Unreleased]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.5.1...HEAD
[0.5.1]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/releases/tag/v0.1.0
