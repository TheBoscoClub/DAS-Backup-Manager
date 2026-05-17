# DAS Backup Manager — ESP Safety (CRITICAL)

## NEVER sync, mirror, copy, or overwrite DAS ESP partitions

The DAS Backup Manager project manages two 2 TB drives in a TerraMaster D6-320 enclosure (bays 1 and 3) that contain **fully independent operating system installations** with their own ESP partitions. Device letters vary on each reconnect — identify by serial or label:

- Serial ZK208Q77 (bay 1): p1 (1.5G, vfat, LABEL=BACKUP-ESP) + p2 (das-backup-system-recovery-A; was `das-backup-system-mirror` prior to the 2026-05-17 rename)
- Serial ZFL41DNY (bay 4, was bay 3 prior to 2026-05-06): p1 (1.5G, vfat, LABEL=BACKUP-ESP) + p2 (das-backup-system-recovery-B; was `das-backup-system` prior to the 2026-05-17 rename)

### HARD RULES — No Exceptions

1. **NEVER use esp-sync, rsync, cp, dd, or ANY tool to write to BACKUP-ESP partitions** from the host system
2. **NEVER create pacman hooks that sync the host ESP to DAS drives**
3. **NEVER include DAS drives in any ESP mirroring, backup, or sync operation**
4. **The DAS ESPs are TOTALLY independent** of the host system's ESP — they boot their own OS installations
5. **esp-sync.sh and any ESP sync hooks MUST only operate on**:
   - Primary ESP: `/dev/nvme0n1p3` (LABEL=EFI, mounted at /boot)
   - Backup ESP: `/dev/nvme1n1p3` (LABEL=EFI-BACKUP, mounted at /mnt/esp-backup)

### 2026-03-05 Incident — Root Cause (identified 2026-04-10)

esp-sync destroyed both DAS emergency boot/recovery ESPs by overwriting them with the host system's ESP content. The DAS drives' independent OS boot configurations were lost.

**Root cause**: DAS-Backup-Manager's `indexer/src/setup/templates.rs` contained a `render_esp_hook()` function that — every time `btrdasd setup --upgrade` ran — generated `/etc/pacman.d/hooks/das-esp-sync.hook` pointing at `/usr/lib/das-backup/esp-sync.sh`. The referenced script (no longer present in the repo) discovered ESP partitions by label/filesystem and mirrored the host ESP onto all of them, including BACKUP-ESP on the DAS 2TB drives. A kernel update triggered pacman hooks on 2026-03-05 and the script ran, destroying the DAS drives' independent OS boot configs.

The emergency response disabled `[esp] enabled = false` in `config.toml` but left `[esp.hooks] enabled = true` untouched — and left the `render_esp_hook()` template code in place. Every subsequent `btrdasd setup --upgrade` silently regenerated the orphan hook. The missing script meant the hook was harmless in that window (pacman would error on kernel updates), but any reintroduction of `esp-sync.sh` at the referenced path would have repeated the disaster.

**Fix (2026-04-10)**: `render_esp_hook()` and its caller were deleted from `templates.rs`. The `EspHooks` struct and `HookType` enum were removed from `indexer/src/config.rs`. The wizard's ESP hook auto-detect/prompt block was removed from `setup/wizard.rs`. The live `[esp.hooks]` block was removed from `/etc/das-backup/config.toml`. The orphan `das-esp-sync.hook` was deleted from `/etc/pacman.d/hooks/`. DAS-Backup-Manager no longer contains any code path that can generate an ESP sync hook, pacman or otherwise.

**Orthogonal NVMe mirror sync** (primary NVMe ESP → backup NVMe ESP) is handled by a completely separate mechanism outside this project: `/usr/local/bin/esp-sync.sh` (package-unowned, hand-installed) driven by `/etc/pacman.d/hooks/esp-mirror.hook`. That mechanism targets only the two NVMe ESPs (LABEL=EFI ↔ LABEL=EFI-BACKUP) and is what keeps the mirrored boot disks bootable if one NVMe dies. It has no connection to DAS drives and is not the 3/5 vector. **Do not conflate the two systems.**

### Identification

DAS backup drives are identifiable by:
- Labels: `BACKUP-ESP` (ESP partitions on both 2TB drives), `das-backup-system-recovery-A`, `das-backup-system-recovery-B`, `das-backup-22tb`
- Serials: `ZK208Q77` (bay 1), `ZXA1R71M` (bay 2, RMA replacement for failed `ZXA0LMAE` — installed 2026-05-15), `ZFL41DNY` (bay 4, was bay 3 prior to 2026-05-06), `ZXA1NYGZ` (bay 5, RAID-1 partner of `ZXA1R71M` in the 22TB array; added 2026-05-06 originally as partner of `ZXA0LMAE`)
- Device paths: vary on reconnect (USB-attached TerraMaster D6-320 enclosure)
- Mount points: `/mnt/backup-22tb`, `/run/media/bosco/das-*`

Note: `dasRaid0` was relocated to internal SATA (2026-04-06) and is no longer in the DAS enclosure.
