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
- **A pair follows its subvolume by the subvolume's own btrfs UUID** (bd `sp78.5`; `btrfs subvolume list -u`,
  `coverage::follow_renames`): `mv @a @b` is one pair moved, and a rename into an excluded name is a drop, not a
  retirement. A row with no UUID, a volume read without one, or a UUID two paths carry is paired by path alone,
  never as covered.
- **A volume follows its filesystem UUID** (bd `sp78.6`; the `findmnt` UUID the sync already verifies,
  `VolumeListing::fs_uuid`): the same filesystem mounted at a new `[[source]].volume` is the same pairs, only
  when exactly one read volume holds it and the row's own path does not; otherwise paired by path.
- **Delivery is the doctor's check, never coverage's or the nightly run's** (bd `sp78.7`, operator 2026-11-09):
  `doctor::delivery_check` lists each verified target's directory for every pair `btrbk.conf` declares; a pair
  with no `<snapshot name>.<timestamp>` newer than `[doctor].max_snapshot_age_days` (default 3) is UNDELIVERED
  (exit 1), and a target that could not be verified, mounted or listed is TARGETS NOT CHECKED (exit 3), never fresh.
- **Coverage changes no exit code.** A check that cannot run is WARN `not checked: …`; the one FAIL is
  `backup-run.sh` reading no recognised `SUBVOLUME COVERAGE:` row from a sync that returned 0 or 1. The row
  is `STATUS  (detail)`, two spaces, on both paths: script and binary ship together.
- **No test reaches the host's index database**: every test sync names its own `--db` or `CoverageMode` path.
