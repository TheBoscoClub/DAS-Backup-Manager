# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Project scaffolding with CMake build system (ECM + Qt6 + KF6)
- Migrated backup scripts from CachyOS-Kernel project
  - `backup-run.sh` v3.1.0 — btrbk backup with throughput logging + email report
  - `backup-verify.sh` v2.0.0 — DAS drive health + btrbk status verification
  - `btrbk.conf` — reference btrbk configuration
  - `das-partition-drives.sh` — DAS drive partitioning utility
  - `install-backup-timer.sh` — systemd timer installer
- Migrated systemd units: das-backup.service/timer, das-backup-full.service/timer
- Migrated documentation: OFFLINE-BACKUP-PLAN.md, DISASTER-RECOVERY-GUIDE.md, STORAGE-ARCHITECTURE-AND-RECOVERY.md, DAS-BAY-MAPPING.md
- GitHub repo with full security: Dependabot, CodeQL, secret scanning, branch protection
- GPL-3.0 license
