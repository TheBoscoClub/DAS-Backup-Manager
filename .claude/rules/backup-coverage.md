---
paths:
  - "indexer/src/coverage.rs"
  - "indexer/src/adopt.rs"
  - "indexer/src/subvol.rs"
  - "scripts/backup-run.sh"
---

# Backup System Rules — Subvolume Coverage

Detail per section: `.claude/docs/rules-reference/backup.md` (same headings, NOT auto-loaded) — **read that section before changing anything this file governs.** Split out of `backup.md` 2026-11-09; loads only when a matching file is read or edited.

## Subvolume Coverage

- **One rule, both paths** (bd `sp78.1`): every sync compares the `(volume, subvolume, target)` pairs
  `config.toml` schedules with the last backup run's record — `adopt::sync_subvolumes` → `coverage::check`,
  never a second copy. Intent, never delivery.
- **Only a real backup run records** (`--record-coverage`, `SystemBackupHost::sync`); a dry run, a hand
  `subvol sync` and `backup snapshot|send` write nothing and never create a database.
- A recorded pair no longer scheduled and still on disk is a **WARN naming the rule that dropped it**, every
  run until restored, gone from disk, or accepted per pair (`btrdasd subvol coverage accept`), never per
  pattern. A subvolume gone from a volume that was read is retirement, never a WARN.
- **Coverage changes no exit code.** A check that cannot run is WARN `not checked: …`; the one FAIL is
  `backup-run.sh` reading no recognised `SUBVOLUME COVERAGE:` row from a sync that returned 0 or 1. The row
  is `STATUS  (detail)`, two spaces, on both paths: script and binary ship together.
- **No test reaches the host's index database**: every test sync names its own `--db` or `CoverageMode` path.
