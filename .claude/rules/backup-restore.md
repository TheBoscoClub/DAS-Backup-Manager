---
paths:
  - "indexer/src/restore.rs"
  - "indexer/src/config.rs"
  - "indexer/src/bin/btrdasd-helper.rs"
  - "gui/src/restore*"
---

# Backup System Rules — Restore Destination Policy

Detail per section: `.claude/docs/rules-reference/backup.md` (same headings, NOT auto-loaded) — **read that section before changing anything this file governs.** Split out of `backup.md` 2026-11-09; loads only when a matching file is read or edited.

## Restore Destination Policy (since 0.7.20.0)

Checked before any directory is created, both roots compared **after resolution** (an allowed root only
through links root owns, bd `sp78.18`):

- **`[restore] allowed_roots`** in `config.toml` — default `/home`, `/tmp`. Widen it there, not in code;
  grant a subdirectory, never `/srv` itself.
- **`RESTORE_DENIED_ROOTS`** (`indexer/src/config.rs`) is checked first and **no configuration can
  override it** — system paths plus served roots `/srv/http` and `/srv/ftp`.
- **No copied file or link goes through a link, but one kind is followed.** Every write of the copy paths is
  made under a directory fd reached one name at a time, each opened `O_NOFOLLOW` (bd `fcmk`): a link on the
  way, planted or swapped in after the check, is refused; `btrfs receive` gets the held directory's
  `/proc/<pid>/fd/<n>` (bd `sp78.18`). The destination itself is always reached strictly.
- **The one link followed** (bd `mz3z`, operator 2026-11-09): **below** a destination a user owns (one root
  owns — the restore made it — follows none), a link that user owns (never root's: the restore makes root-owned
  links itself, with targets from the snapshot), owner and target read from one inode (`O_PATH`), whose target —
  every link on its way owned by root or that user — resolves under an `allowed_roots` entry and no denied root,
  to an existing directory that user owns. That directory is re-opened from `/` by the strict walk; the link is
  never traversed, and wherever a rename on the way lands the resolution, root writes only into a directory the
  destination's owner owns. Any other link is refused, naming the target and, when a grant would allow it, the
  `allowed_roots` entry to add; at most 8 per walk.
  Absolute or non-literal member paths are rejected.
