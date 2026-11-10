---
paths:
  - "indexer/src/backup.rs"
  - "indexer/src/main.rs"
  - "indexer/src/bin/btrdasd-helper.rs"
  - "indexer/src/mount.rs"
  - "gui/src/**"
  - "dbus/**"
---

# Backup System Rules — The CLI/GUI Backup Touches Only What Was Selected, And Mounts Nothing Itself

Detail per section: `.claude/docs/rules-reference/backup.md` (same headings, NOT auto-loaded) — **read that section before changing anything this file governs.** Split out of `backup.md` 2026-11-09; loads only when a matching file is read or edited.

## The CLI/GUI Backup Touches Only What Was Selected, And Mounts Nothing Itself (bd `7tx`)

- btrbk gets **only** the selected (subvolume, target) pairs, one `<target dir>/<snapshot_name>` filter
  each — never a volume path, never a source filter beside a target filter (filters are a union). No
  filter only when the selection IS everything.
- **An empty selection, or a label the configuration lacks, is refused** before the first lock or
  mount (exit 1) — never dropped, never widened to "all". The GUI's D-Bus lists included.
- `backup snapshot` and `backup send` sync `btrbk.conf` first, as `run` does; a failed sync stops them.
  `run` goes on after a failed sync only while `btrbk.conf` is still what `config.toml` renders (bd `03j3`).
- The run mounts **nothing** itself: each `MountGuard` gives back exactly what it mounted;
  `run_backup` only refuses an unmounted selected source.
- The job mounts only the selected targets (none named → all; `selected_targets_only`, bd `u8kk`).
- Never re-render `btrbk.conf` per run to narrow the targets (it shifts other targets' retention).

## The CLI/GUI Run Records Truthfully, And Exits By The Doctor's Rule (bd `no4`, `vzsu`)

- A count a failed step could not take is `None` (NULL / `unknown` / `null`), never `0`.
- `btrdasd backup run` exits 0 / 3 / 1 by the script's rule (`BackupJobOutcome::exit_code`); an aborted
  run records a failed row; a report neither saved nor mailed fails the run before the row is written.
- A failed `host.record` fails the job, never only warns.
- `backup run` records the selected recovery OSes it mounted (`recovery-os.json`, bd `58c8`), one rule
  with `check_recovery_os`: STALE/WARNING is WARN; unreadable, unwritable or timed out is FAIL.
  `btrdasd recovery-os record` does it without a backup.
