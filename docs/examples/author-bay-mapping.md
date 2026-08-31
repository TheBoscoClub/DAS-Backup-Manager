> **Note**: This is the author's specific configuration. See [DAS-BAY-MAPPING.md](../DAS-BAY-MAPPING.md) for the generic guide.

# TerraMaster D6-320 Bay Mapping

**Date mapped**: 2026-02-04
**Updated**: 2026-05-06 (added second 22TB CMR drive in bay 5; das-backup-22tb converted from single to BTRFS RAID-1)
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
|              |  RAID-1 leg 1|                   |
+--------------+--------------+-------------------+
|    Bay 4     |    Bay 5     |    Bay 6          |
|   ZFL41DNY   |   ZXA1NYGZ   |   (empty)         |
|   2TB SMR    | * 22TB CMR   |                   |
|  Emergency   |  PRIMARY     |                   |
|  Boot/Recov  |  BACKUP      |                   |
|              |  RAID-1 leg 2|                   |
+--------------+--------------+-------------------+
```

## Drive Details

| Bay | Serial | Model | Size | Partitions | Role | BTRFS Label |
|-----|--------|-------|------|------------|------|-------------|
| 1 | ZK208Q77 | ST2000DM008 | 1.8T | p1 (ESP) + p2 (BTRFS) | Emergency Boot/Recovery + btrbk NVMe/SSD target (independent recovery copy A) | das-backup-system-recovery-A |
| 2 | ZXA1R71M | ST22000NM000C (Exos) | 20T | p1 (BTRFS, whole disk) | Primary Backup — RAID-1 leg 1 — all btrbk targets (RMA replacement for `ZXA0LMAE` since 2026-05-15) | das-backup-22tb |
| 3 | — | — | — | — | Empty | — |
| 4 | ZFL41DNY | ST2000DM008 | 1.8T | p1 (ESP) + p2 (BTRFS) | Emergency Boot/Recovery + btrbk NVMe/SSD target (independent recovery copy B) | das-backup-system-recovery-B |
| 5 | ZXA1NYGZ | ST22000NM000C (Exos) | 20T | p1 (BTRFS, whole disk) | Primary Backup — RAID-1 leg 2 — all btrbk targets | das-backup-22tb |
| 6 | — | — | — | — | Empty | — |

## Primary Backup — BTRFS RAID-1 Across Bays 2 & 5 (originally added 2026-05-06; restored 2026-05-16 after RMA replacement of failed `ZXA0LMAE`)

The two 22TB CMR drives in bays 2 and 5 form a single BTRFS RAID-1 filesystem. Both drives must be online for the backup target to mount; this trades the offline/air-gap model for live redundancy against single-drive failure during the multi-day recovery window of a 22TB drive replacement.

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
- Mount: `/mnt/backup-22tb` (production) / `/run/media/bosco/das-backup-22tb` (auto-mounted)

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

Either drive can boot independently if the other fails. Sync between them is manual (btrbk send/receive or similar), not automatic.

> **ESP label history**: both ESPs were originally labelled `BACKUP-ESP` and were
> relabelled in place to the bay-numbered `RECOV-ESP-<bay>` form. The vfat UUIDs
> above are unchanged, which is how the relabel is distinguishable from a
> reformat — the filesystems are the originals. The bay-numbered form is the
> correct one to keep: a single shared label made
> `blkid -t LABEL=BACKUP-ESP -o device` return *both* partitions, so any lookup
> had to guess which recovery system it meant. `scripts/das-partition-drives.sh`
> derives this label per drive and refuses to run if two targets would collide.

## Role Summary

- **Primary Backup** (Bays 2 + 5): 2x 22TB Exos in BTRFS RAID-1 — all btrbk targets (NVMe, SSD, projects, audiobooks, das-storage), deep retention. Single-drive failure does not lose data; replacement happens online via `btrfs replace`.
- **Emergency Boot/Recovery** (Bays 1, 4): Independent 2TB drives with ESP + CachyOS — also receive btrbk NVMe/SSD snapshots. No mutual redundancy.

## dasRaid0 — Relocated to Internal SATA (2026-04-06)

The BTRFS RAID0 general storage array was moved from DAS bays 3/4/5 to internal PC SATA connections. A 4th drive was added internally.

- **Label**: dasRaid0
- **UUID**: d29fdda7-a1e5-4640-996e-2b78569cb65d
- **Mount**: /dasRaid0
- **Members**: 4x ST2000DM008 (was 3, expanded with ZFL41DV0, ZK208Q7J, ZK208RH6 + 1 additional)
- **Data profile**: RAID0 (striped)
- **Metadata profile**: RAID1
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
- LED identification: `sudo dd if=/dev/sdX of=/dev/null bs=1M count=2000 status=progress`
- 22TB Exos drives are CMR (conventional magnetic recording) — no SMR write penalties
- 2TB drives: all ST2000DM008 (SMR), same batch March 2021, ~13,000 hours each
- 22TB drives: ST22000NM000C — original `ZXA0LMAE` sourced 2026-02 (failed, RMA'd) + `ZXA1NYGZ` sourced 2026-05 + `ZXA1R71M` arrived 2026-05-15 as RMA replacement for `ZXA0LMAE` (factory recertified 2025-08-05, ~45h burn-in). Current RAID-1 pair is `ZXA1NYGZ` + `ZXA1R71M`, sourced from different production batches to mitigate correlated-failure risk
- USB topology: each bay gets an independent USB sub-device via the enclosure's bridge chip (4-1.3.x)
- 2026-05-06 bay reshuffle: ZFL41DNY moved from bay 3 → bay 4 to make room for the new 22TB CMR drive in bay 5
