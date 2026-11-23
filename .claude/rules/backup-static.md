---
paths:
  - "indexer/src/static_send.rs"
  - "indexer/src/config.rs"
  - "indexer/src/backup.rs"
  - "indexer/src/main.rs"
  - "indexer/tests/subvol_sync_loopback.rs"
  - "scripts/backup-run.sh"
  - "tests/test_subvol_sync.sh"
  - "tests/test_backup_exit_semantics.sh"
---

# Backup System Rules — Static Subvolumes

Detail per section: `.claude/docs/rules-reference/backup.md` (same headings, NOT auto-loaded) — **read that section before changing anything this file governs.** Split out of `backup.md` 2026-11-23 (instruction-size limit, moved verbatim); loads only when a matching file is read or edited.

## Static Subvolumes (bd `5237.8`)

Reasons, probes and the identity rule: `.claude/docs/rules-reference/backup.md` (same heading).

- A `[[source.static]]` table → read-only subvolumes somebody else made, sent to the source's primary targets as they are by `btrdasd backup static` (`backup-run.sh` after the expiry, under its handed-down singleton; `backup run`; a hand run takes the locks, mounts and sync as `backup send`, a dry run does not sync).
- Source side → never a snapshot, a property change or a delete; a read-write candidate → WARN, never sent, never made read-only.
- A subvolume's identity → its received UUID when set, else its UUID; a copy belongs to the candidate whose identity it carries; a copy's source is gone only when no subvolume anywhere on the source volume has that identity. One function (`Subvol::identity`).
- The incremental parent → the newest read-only sibling (same parent UUID, highest id) on the whole source volume that the target holds a read-only received copy of, anywhere on the target; else a full send.
- A copy is received at its final name in `target_subdir` → never renamed after (a received read-only subvolume cannot be moved: EROFS); a marker file in `<target_subdir>/.incoming/` is written before each receive and removed once the copy is checked. Never clear `ro` to move one.
- A copy counts only when `btrfs subvolume show` reads back its received UUID equal to the identity and `readonly`; the same name taken by anything else → FAIL for that one, nothing overwritten; a destination found by `symlink_metadata` → refused.
- The only two deletes, each `btrfs subvolume delete` on one exact path from a verified target mount, never recursive, never on the source: (a) at the start of a pass, a read-write subvolume with no received UUID directly in `target_subdir` whose marker is in `.incoming/`; (b) a read-only copy with a received UUID whose source is gone, once a current candidate of the same series has a checked copy in a fresh listing taken after the sends, with no candidate of that series unsettled, the target verified again just before, and the pattern ending in `*` after a literal.
- Everything else in `target_subdir` (no received UUID, read-write, no marker) → kept and listed, never deleted.
- An unreadable or empty listing, either side → refusal for that table, never "nothing there"; a target not mounted at its top level, or a symlink in the `target_subdir` path → refusal.
- `target_subdir` → distinct from, and not nested with, every directory a source sends snapshots to and every other table's; `dir` → inside a `.snapshots`/`.btrbk-snapshots` tree (sync adopts all else); `pattern` → never `.archive` (`boot-archive-cleanup.sh`). `Config::static_errors` enforces all three.
- A failed table → FAIL in `STATIC SUBVOLUMES` (run exit 3) and stops nothing after it.
