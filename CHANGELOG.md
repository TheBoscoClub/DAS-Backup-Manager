# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
- GPL-3.0 license
