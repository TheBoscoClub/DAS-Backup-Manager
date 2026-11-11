---
paths:
  - "scripts/backup-run.sh"
  - "scripts/boot-archive-cleanup.sh"
  - "indexer/src/backup.rs"
  - "indexer/src/forget.rs"
  - "indexer/src/config.rs"
---

# Backup System Rules — Boot Subvolume Archival

Detail per section: `.claude/docs/rules-reference/backup.md` (same headings, NOT auto-loaded) — **read that section before changing anything this file governs.** Split out of `backup.md` 2026-11-09; loads only when a matching file is read or edited.

## Boot Subvolume Archival

- Snapshot live `@`/`@home` read-only to `@.archive.<TS>` / `@home.archive.<TS>`, **then** replace the
  live subvolume from the latest btrbk snapshot. Archive failed ⇒ replacement skipped — the only copy
  of the outgoing subvolume is never destroyed.
- Two code paths, kept symmetric: bash `update_boot_subvolumes()` (`scripts/backup-run.sh`) and Rust
  `archive_boot_with` (`indexer/src/backup.rs`). **Both locate the replacement BEFORE deleting anything**;
  none found is a WARN.
- **One classification, both paths** (bd woq, dtm): an absence is WARN; anything meant to work that did
  not is FAIL and fails the run (exit 3); every check runs before the first mutation. An incremental run
  creates a missing boot subvolume, never replaces one; only a full run (or `backup boot-archive`)
  archives and replaces.
- The boot step validates its own input (`Config::boot_input_errors`; `Config::load` does not) before any btrfs call.
- A boot subvolume is listed by one source: a second (retired or not) → the boot step FAILs, nothing touched (bd `5237.12`).
- **Snapshot names are read from `/etc/btrbk/btrbk.conf`, never re-derived**
  (`forget::live_subvol_snapshot_names()`). Unreadable ⇒ whole step FAILs.
- **Both paths, and the pruner, skip `role=mirror` targets entirely** — recovery drives carry their own
  OS under `@`/`@home`. Ordinary btrbk send/receive to them is unaffected.
- The pruner deletes only `@.archive.*` / `@home.archive.*` past retention on non-mirror targets.
  Old archives already on the mirrors are left for manual review, never auto-removed.
