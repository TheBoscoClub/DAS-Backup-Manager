# DAS Backup Manager — ESP Safety (CRITICAL)

## NEVER sync, mirror, copy, or overwrite DAS ESP partitions

The DAS Backup Manager project manages two 2 TB drives in a TerraMaster D6-320 enclosure (bays 1 and 4) that contain **fully independent operating system installations** with their own ESP partitions. Device letters vary on each reconnect — identify by serial or label:

- Serial ZK208Q77 (bay 1): p1 (1.5G, vfat, **LABEL=RECOV-ESP-1**, UUID `6D15-0632`, PARTUUID `fe640619-2c7b-457a-be77-61bc9aff4875`) + p2 (das-backup-system-recovery-A; was `das-backup-system-mirror` prior to the 2026-05-17 rename)
- Serial ZFL41DNY (bay 4, was bay 3 prior to 2026-05-06): p1 (1.5G, vfat, **LABEL=RECOV-ESP-4**, UUID `6CAB-B04D`, PARTUUID `ef19ce6e-de5e-4623-bed0-8717749916b8`) + p2 (das-backup-system-recovery-B; was `das-backup-system` prior to the 2026-05-17 rename)

Both ESPs were labelled `BACKUP-ESP` until relabelled in place (vfat UUIDs
unchanged). A rule keyed to a label that exists nowhere cannot fire, and a shared
label is itself a defect: `scripts/das-partition-drives.sh` (v2.1.0+) derives the
unique bay-numbered label and refuses on collision (`tests/test_esp_label_derivation.sh`).
Full account: `.claude/docs/rules-reference/esp-safety.md`.

### HARD RULES — No Exceptions

1. **NEVER use esp-sync, rsync, cp, dd, or ANY tool to write to the `RECOV-ESP-*` partitions** (formerly labelled `BACKUP-ESP`) from the host system. Identify them by serial (`ZK208Q77`, `ZFL41DNY`) or PARTUUID as well as by label — a label is renameable, so it is the weakest of the three identifiers
2. **NEVER create pacman hooks that sync the host ESP to DAS drives**
3. **NEVER include DAS drives in any ESP mirroring, backup, or sync operation**
4. **The DAS ESPs are TOTALLY independent** of the host system's ESP — they boot their own OS installations. That OS writing its own ESP while booted (bare metal, or the recovery-os-updater VM with its whole disk attached by serial) is the drive's own OS acting, and permitted; the host still never writes `RECOV-ESP-*` and never mounts a drive the VM holds
5. **esp-sync.sh and any ESP sync hooks MUST only operate on**:
   - Primary ESP: LABEL=EFI, mounted at /boot (`nvme1n1p3` on 2026-10-02 — NVMe numbers are not stable)
   - Backup ESP: LABEL=EFI-BACKUP, mounted at /mnt/esp-backup

### 2026-03-05 Incident — Root Cause (identified 2026-04-10)

esp-sync destroyed both DAS recovery ESPs by mirroring the host ESP onto them.
The vector was this project's own `render_esp_hook()` (`indexer/src/setup/templates.rs`),
which regenerated a pacman hook on every `setup --upgrade`. It, `EspHooks`/`HookType`
and the wizard's hook prompt were deleted 2026-04-10. **No code path here can
generate an ESP sync hook — keep it that way.**

**The NVMe mirror is a different system — never conflate them.**
`/usr/local/bin/esp-sync.sh` via `/etc/pacman.d/hooks/esp-mirror.hook` syncs only
`/boot` → `/mnt/esp-backup`: hardcoded paths, no discovery loop, a label↔mount
cross-check, and a hard refusal of any device not matching `/dev/nvme*` — the
load-bearing guard; the USB DAS ESPs are refused by bus class, not by name. It
deletes from the backup any file `/boot` lacks, so `/boot` must be complete first.
Bootloader installation on the primary ESP is CachyOS-Kernel's (`esp-ownership.md`).

Full postmortem and the 2026-08-31 re-verification of those guards:
`.claude/docs/rules-reference/esp-safety.md`.

### Identification

DAS backup drives are identifiable by:

- Labels: `RECOV-ESP-1` / `RECOV-ESP-4` (ESP partitions on the bay 1 / bay 4 2TB drives; both were `BACKUP-ESP` historically), `das-backup-system-recovery-A`, `das-backup-system-recovery-B`, `das-backup-22tb`
- Serials: `ZK208Q77` (bay 1), `ZXA1R71M` (bay 2, RMA replacement for failed `ZXA0LMAE` — installed 2026-05-15), `ZFL41DNY` (bay 4, was bay 3 prior to 2026-05-06), `ZXA1NYGZ` (bay 5, RAID-1 partner of `ZXA1R71M` in the 22TB array; added 2026-05-06 originally as partner of `ZXA0LMAE`)
- Device paths: vary on reconnect (USB-attached TerraMaster D6-320 enclosure)
- Mount points: `/mnt/backup-22tb`, `/mnt/backup-system-recovery-A`, `/mnt/backup-system-recovery-B` — mounted by `backup-run.sh` / `btrdasd` only. Hidden from udisks2 since 2026-10-01: a `/run/media/bosco/das-*` mount means the generated ignore rule is not applying (`sudo btrdasd setup --check`)

Note: `dasRaid0` was relocated to internal SATA (2026-04-06) and is no longer in the DAS enclosure.
