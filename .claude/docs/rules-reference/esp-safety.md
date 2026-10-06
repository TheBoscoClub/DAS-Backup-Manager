# esp-safety.md — reference material

<!-- Reference detail for .claude/rules/esp-safety.md. NOT auto-loaded: the rule file is the binding summary;
     this is the reasoning, incidents and measurements behind it. Moved verbatim 2026-10-05. -->

## (moved from “ESP label correction, 2026-08-31”)

**ESP label correction, 2026-08-31.** Both ESPs were previously labelled
`BACKUP-ESP`, and every document here — including this rule, for months — said
so. They had in fact been relabelled in place to the bay-numbered form above.
The vfat UUIDs are unchanged, which is what proves a relabel rather than a
reformat: these are the original filesystems. Two consequences worth holding on
to:

- **A safety rule keyed to a label that exists nowhere cannot fire.** "Never
  write to `BACKUP-ESP`" matched zero partitions on this host and would have
  read as "no DAS ESPs present" to anything doing a label lookup. Nothing failed
  open in practice only because the live sync mechanism never matched on that
  label (see below) — that was luck in the sense that the rule was not what
  saved it.
- **A single shared label was itself the defect.** `blkid -t LABEL=BACKUP-ESP -o
  device` returned *both* partitions, so any consumer had to guess which
  recovery system it meant. The bay-numbered form is unique per drive and is the
  form to keep. `scripts/das-partition-drives.sh` (v2.1.0+) derives it per drive
  and refuses to run if two targets would collide; `tests/test_esp_label_derivation.sh`
  exercises both the success and the refusal paths.

## (moved from “2026-03-05 Incident — Root Cause (identified 2026-04-10)”)

esp-sync destroyed both DAS emergency boot/recovery ESPs by overwriting them with the host system's ESP content. The DAS drives' independent OS boot configurations were lost.

**Root cause**: DAS-Backup-Manager's `indexer/src/setup/templates.rs` contained a `render_esp_hook()` function that — every time `btrdasd setup --upgrade` ran — generated `/etc/pacman.d/hooks/das-esp-sync.hook` pointing at `/usr/lib/das-backup/esp-sync.sh`. The referenced script (no longer present in the repo) discovered ESP partitions by label/filesystem and mirrored the host ESP onto all of them, including BACKUP-ESP on the DAS 2TB drives. A kernel update triggered pacman hooks on 2026-03-05 and the script ran, destroying the DAS drives' independent OS boot configs.

The emergency response disabled `[esp] enabled = false` in `config.toml` but left `[esp.hooks] enabled = true` untouched — and left the `render_esp_hook()` template code in place. Every subsequent `btrdasd setup --upgrade` silently regenerated the orphan hook. The missing script meant the hook was harmless in that window (pacman would error on kernel updates), but any reintroduction of `esp-sync.sh` at the referenced path would have repeated the disaster.

**Fix (2026-04-10)**: `render_esp_hook()` and its caller were deleted from `templates.rs`. The `EspHooks` struct and `HookType` enum were removed from `indexer/src/config.rs`. The wizard's ESP hook auto-detect/prompt block was removed from `setup/wizard.rs`. The live `[esp.hooks]` block was removed from `/etc/das-backup/config.toml`. The orphan `das-esp-sync.hook` was deleted from `/etc/pacman.d/hooks/`. DAS-Backup-Manager no longer contains any code path that can generate an ESP sync hook, pacman or otherwise.

**Orthogonal NVMe mirror sync** (primary NVMe ESP → backup NVMe ESP) is handled by a completely separate mechanism outside this project: `/usr/local/bin/esp-sync.sh` (package-unowned, hand-installed) driven by `/etc/pacman.d/hooks/esp-mirror.hook`. That mechanism targets only the two NVMe ESPs (LABEL=EFI ↔ LABEL=EFI-BACKUP) and is what keeps the mirrored boot disks bootable if one NVMe dies. It has no connection to DAS drives and is not the 3/5 vector. **Do not conflate the two systems.**

**Why it structurally cannot reach the DAS ESPs** (re-verified 2026-08-31 by reading the installed script):

- It **enumerates nothing.** Source and destination are the hardcoded constants `/boot` and `/mnt/esp-backup`. There is no discovery loop, which is the single most important difference from the deleted 2026-03-05 code — that one searched for ESP-shaped partitions and acted on whatever it found.
- `validate_device()` cross-checks that the device mounted at each path is the same device the expected LABEL resolves to, and `die`s with `REFUSING to sync` on mismatch.
- **It hard-blocks any resolved device not matching `/dev/nvme*`.** This is the load-bearing guard, and it is why the ESP relabel above changed nothing about safety: the DAS ESPs are USB-attached `/dev/sd*` and are refused on device class, not on their name. Labels can be renamed with `fatlabel`; a bus class cannot be renamed into existence.
- It copies `/boot` → `/mnt/esp-backup` per file by `md5sum` and **deletes from the backup any file `/boot` lacks**, except `is_unique_file()`'s (`loader/random-seed`, `loader/.#bootctl*`, `test-sync-trigger`). `/boot` must be complete before it runs; `--dry-run` previews.

**Bootloader installation on the primary ESP** (`bootctl install`, creating or reordering NVRAM entries for `LABEL=EFI`) is CachyOS-Kernel's, not this project's — see `~/.claude/rules/esp-ownership.md` "Where the boundary actually falls". This project owns ESP *partition lifecycle* and every cross-ESP operation; it does not own boot configuration confined to `/boot`.
