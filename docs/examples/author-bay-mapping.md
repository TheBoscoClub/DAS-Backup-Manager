> **Note**: This is the author's specific configuration. See [DAS-BAY-MAPPING.md](../DAS-BAY-MAPPING.md) for the generic guide.

# TerraMaster D6-320 Bay Mapping

**Date mapped**: 2026-02-04
**Updated**: 2026-05-06 (added second 22TB CMR drive in bay 5; das-backup-22tb converted from single to BTRFS RAID-1); 2026-05-15 (bay 2 RMA replacement); re-checked 2026-10-02 against `lsblk`, `btrfs filesystem show` and `/etc/das-backup/config.toml`
**Method**: I/O activity LED identification + serial number verification

## Physical Bay Layout

```
+-------------------------------------------------+
| TerraMaster D6-320 (front view)                 |
+--------------+--------------+-------------------+
|    Bay 1     |    Bay 2     |    Bay 3          |
|   ZK208Q77   |   ZXA1R71M   |   (empty)         |
|   2TB SMR    | * 22TB CMR   |                   |
|  Emergency   |  PRIMARY     |                   |
|  Boot/Recov  |  BACKUP      |                   |
|              |  RAID-1 dev 2|                   |
+--------------+--------------+-------------------+
|    Bay 4     |    Bay 5     |    Bay 6          |
|   ZFL41DNY   |   ZXA1NYGZ   |   (empty)         |
|   2TB SMR    | * 22TB CMR   |                   |
|  Emergency   |  PRIMARY     |                   |
|  Boot/Recov  |  BACKUP      |                   |
|              |  RAID-1 dev 1|                   |
+--------------+--------------+-------------------+
```

## Drive Details

| Bay | Serial | Model | Size | Partitions | Role | BTRFS Label |
|-----|--------|-------|------|------------|------|-------------|
| 1 | ZK208Q77 | ST2000DM008 | 1.8T | p1 (ESP) + p2 (BTRFS) | Emergency Boot/Recovery (independent OS) + btrbk target for `nvme`, `ssd`, `projects`, `hdd-system` (recovery A) | das-backup-system-recovery-A |
| 2 | ZXA1R71M | ST22000NM000C (Exos) | 20T | p1 (BTRFS, whole disk) | Primary Backup — RAID-1 devid 2 — every btrbk stream (RMA replacement for `ZXA0LMAE` since 2026-05-15) | das-backup-22tb |
| 3 | — | — | — | — | Empty | — |
| 4 | ZFL41DNY | ST2000DM008 | 1.8T | p1 (ESP) + p2 (BTRFS) | Emergency Boot/Recovery (independent OS) + btrbk target for `nvme`, `ssd`, `projects`, `hdd-system` (recovery B) | das-backup-system-recovery-B |
| 5 | ZXA1NYGZ | ST22000NM000C (Exos) | 20T | p1 (BTRFS, whole disk) | Primary Backup — RAID-1 devid 1 — every btrbk stream | das-backup-22tb |
| 6 | — | — | — | — | Empty | — |

## Primary Backup — BTRFS RAID-1 Across Bays 2 & 5 (originally added 2026-05-06; restored 2026-05-16 after RMA replacement of failed `ZXA0LMAE`)

The two 22TB CMR drives in bays 2 and 5 form a single BTRFS RAID-1 filesystem. `[das].mount_opts` includes `degraded`, so the target still mounts — and backups and restores still run — with one drive missing; data written then lands as `single` chunks until the leg is replaced (scrub, then `btrfs balance start -dconvert=raid1 -mconvert=raid1`). This trades the offline/air-gap model for live redundancy against single-drive failure during the multi-day recovery window of a 22TB drive replacement. Procedure: `docs/DISASTER-RECOVERY-GUIDE.md`, Scenario D.

| | Bay 2 (ZXA1R71M) | Bay 5 (ZXA1NYGZ) |
|---|---|---|
| **Partition** | `p1` — whole disk (sectors 2048–42970644446), GPT type 8300, name `das-backup-22tb` | `p1` — identical layout |
| **PARTUUID** | `099edf5b-e35e-4c0c-86fa-0837a6ebbd73` | `b24e0ea8-fd90-4a36-8c76-26587a29755b` |
| **BTRFS devid** | 2 | 1 |
| **BTRFS UUID_SUB** | `68a45e02-54dc-4d76-a626-6ebe9a084879` | `b72e7628-c9e3-4b04-87dd-55e253ecaec3` |

**Filesystem-level identifiers** (shared across both devices):
- BTRFS UUID: `b2dbe07d-40b9-422e-8ccf-ef4931c40457`
- Label: `das-backup-22tb`
- Profiles: Data RAID-1, Metadata RAID-1, System RAID-1
- Mount: `/mnt/backup-22tb`, by `mount_uuid` (`UUID=b2dbe07d-…`), by `backup-run.sh` and `btrdasd` only — never fstab. Hidden from udisks2 since 2026-10-01, so it no longer appears under `/run/media/bosco/`

**Adding a second leg** (the 2026-05-06 conversion, for reference):
```bash
# Match partition geometry exactly to existing leg
sudo sgdisk --new=1:2048:42970644446 --typecode=1:8300 \
    --change-name=1:das-backup-22tb /dev/<new-leg>
sudo partprobe /dev/<new-leg>

# Add device to filesystem
sudo btrfs device add /dev/<new-leg>1 /mnt/backup-22tb

# Convert all profiles to RAID-1 (data, metadata, system)
sudo btrfs balance start -dconvert=raid1 -mconvert=raid1 -sconvert=raid1 \
    --force /mnt/backup-22tb

# Verify after balance completes (no mixed profiles)
sudo btrfs filesystem df /mnt/backup-22tb
sudo btrfs scrub start -B /mnt/backup-22tb  # Verifies both copies
```

## Emergency Boot/Recovery Drives

The two 2TB drives in bays 1 and 4 are **independent standalone bootable systems** — NOT a BTRFS RAID-1 pair. Each has its own ESP and its own BTRFS root filesystem with a separate UUID:

| | Bay 1 (ZK208Q77) | Bay 4 (ZFL41DNY) |
|---|---|---|
| **ESP** | `p1` — 1.5G FAT32, label `RECOV-ESP-1`, UUID `6D15-0632` | `p1` — 1.5G FAT32, label `RECOV-ESP-4`, UUID `6CAB-B04D` |
| **BTRFS** | `p2` — label `das-backup-system-recovery-A`, UUID `60b05268-7f8f-47b5-a38a-752576a1172a` | `p2` — label `das-backup-system-recovery-B`, UUID `7c7ae72d-09d6-4086-b249-1ac60f21b73b` |

Either drive can boot independently if the other fails. Nothing syncs one recovery OS to the other, and nothing may write the host ESP onto either (`.claude/rules/esp-safety.md`). Each receives the host's btrbk snapshots separately (7 daily) under `nvme/`, `ssd/`, `projects/` and `hdd-system/`; their own `@`/`@home` are their OS, untouched by the boot-subvolume refresh. Both mount by `mount_uuid` at `/mnt/backup-system-recovery-A` and `/mnt/backup-system-recovery-B`.

| | Bay 1 (ZK208Q77) | Bay 4 (ZFL41DNY) |
|---|---|---|
| **ESP PARTUUID** | `fe640619-2c7b-457a-be77-61bc9aff4875` | `ef19ce6e-de5e-4623-bed0-8717749916b8` |
| **BTRFS PARTUUID** | `338aa641-de0a-4c70-871c-128ddf32804f` | `f612c94e-991f-42c1-9ce8-faf63829fd2c` |
| **BTRFS label before 2026-05-17** | `das-backup-system-mirror` | `das-backup-system` |

> **ESP label history**: both ESPs were originally labelled `BACKUP-ESP` and were
> relabelled in place to the bay-numbered `RECOV-ESP-<bay>` form. The vfat UUIDs
> above are unchanged, which is how the relabel is distinguishable from a
> reformat — the filesystems are the originals. The bay-numbered form is the
> correct one to keep: a single shared label made
> `blkid -t LABEL=BACKUP-ESP -o device` return *both* partitions, so any lookup
> had to guess which recovery system it meant. `scripts/das-partition-drives.sh`
> derives this label per drive and refuses to run if two targets would collide.

## Role Summary

- **Primary Backup** (Bays 2 + 5): 2x 22TB Exos in BTRFS RAID-1 — every btrbk stream (`nvme`, `nvme-vm`, `ssd`, `ssd-steam`, `ssd-vm`, `projects`, `hdd-media`, `hdd-system`, `audiobooks`, `das-storage`, plus any `<source>-adopted` directory a backup run creates for a newly found subvolume), retention 7 daily / 4 weekly / 12 monthly / 1 yearly. A retired subvolume's snapshots are kept for 372 days after the retirement date (the longest tier, 12 monthly × 31 days) and deleted by the first `backup-run.sh` run on or after day 373. Single-drive failure does not lose data; replacement happens online via `btrfs replace`.
- **Emergency Boot/Recovery** (Bays 1, 4): Independent 2TB drives with ESP + CachyOS — also receive the `nvme`, `ssd`, `projects` and `hdd-system` btrbk snapshots (7 daily). No mutual redundancy.

## dasRaid0 — Relocated to Internal SATA (2026-04-06)

The BTRFS RAID0 general storage array was moved from DAS bays 3/4/5 to internal PC SATA connections. A 4th drive was added internally.

- **Label**: dasRaid0
- **UUID**: d29fdda7-a1e5-4640-996e-2b78569cb65d
- **Mount**: /dasRaid0
- **Members**: 4x ST2000DM008 — `ZK208RH6`, `ZFL41DV0`, `ZK208Q7J` (the original three) + `ZK30JJ2Z` (ST2000DM008-2UB102, added when moved)
- **Data profile**: RAID0 (striped)
- **Metadata profile**: RAID1C3 (as of 2026-10-02)
- **Reason for move**: Direct SATA connections provide better performance than USB bridge

## Offline/Removed Drives

| Serial | Model | Size | Former Role | Status |
|--------|-------|------|-------------|--------|
| ZFL416F6 | ST2000DM008 | 1.8T | DAS Bay 4 (unused) | Removed 2026-04-06, stored offline as cold spare for dasRaid0 |
| W4J1AEY1 | ST5000DM000 | 4.5T | DAS Bay 6 (Scratch) | Removed 2026-04-06, scrapped (unreliable due to age — 30,157 hours) |
| ZK208RH6 | ST2000DM008 | 1.8T | DAS Bay 3 (dasRaid0 1/3) | Moved 2026-04-06 to internal SATA (dasRaid0 member) |
| ZFL41DV0 | ST2000DM008 | 1.8T | DAS Bay 4 (dasRaid0 2/3) | Moved 2026-04-06 to internal SATA (dasRaid0 member) |
| ZK208Q7J | ST2000DM008 | 1.8T | DAS Bay 5 (dasRaid0 3/3) | Moved 2026-04-06 to internal SATA (dasRaid0 member) |

## Notes

- **Device letters change on every reboot/reconnect** — always identify by serial number
- LED identification: `sudo dd if=/dev/disk/by-id/ata-<model>_<serial> of=/dev/null bs=1M count=2000 status=progress` (read-only; the `by-id` name ties the blinking bay to the serial)
- 22TB Exos drives are CMR (conventional magnetic recording) — no SMR write penalties
- 2TB drives: all ST2000DM008 (SMR); the original ones from one batch, March 2021. The recovery drives read 18,304 h (`ZK208Q77`) and 18,315 h (`ZFL41DNY`) of power-on time in the 2026-10-02 report
- 22TB drives: ST22000NM000C — original `ZXA0LMAE` sourced 2026-02 (failed, RMA'd) + `ZXA1NYGZ` sourced 2026-05 + `ZXA1R71M` arrived 2026-05-15 as RMA replacement for `ZXA0LMAE` (factory recertified 2025-08-05, ~45h burn-in). Current RAID-1 pair is `ZXA1NYGZ` + `ZXA1R71M`, sourced from different production batches to mitigate correlated-failure risk
- USB topology: each bay gets an independent USB sub-device (product `TDAS`, vendor string `TerraMas`) via the enclosure's bridge chip. On 2026-10-02 they enumerated at `2-2.3.1` (bay 1), `2-2.3.2` (bay 4), `2-2.3.4` (bay 2) and `2-2.4` (bay 5), all at 10000 Mbit/s (link speed); the path changes with the port used and changed with the 2026-08-29 board swap
- 2026-05-06 bay reshuffle: ZFL41DNY moved from bay 3 → bay 4 to make room for the new 22TB CMR drive in bay 5
