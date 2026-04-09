> **Note**: This is the author's specific configuration. See [DAS-BAY-MAPPING.md](../DAS-BAY-MAPPING.md) for the generic guide.

# TerraMaster D6-320 Bay Mapping

**Date mapped**: 2026-02-04
**Updated**: 2026-04-06 (dasRaid0 moved to internal SATA; bays 4-6 cleared)
**Method**: I/O activity LED identification + serial number verification

## Physical Bay Layout

```
+-------------------------------------------------+
| TerraMaster D6-320 (front view)                 |
+--------------+--------------+-------------------+
|    Bay 1     |    Bay 2     |    Bay 3          |
|   ZK208Q77   |   ZXA0LMAE   |   ZFL41DNY        |
|   2TB SMR    | * 22TB CMR   |   2TB SMR          |
|  Emergency   |  PRIMARY     |  Emergency        |
|  Boot/Recov  |  BACKUP      |  Boot/Recov       |
+--------------+--------------+-------------------+
|    Bay 4     |    Bay 5     |    Bay 6          |
|   (empty)    |   (empty)    |   (empty)         |
|              |              |                    |
|              |              |                    |
|              |              |                    |
+--------------+--------------+-------------------+
```

## Drive Details

| Bay | Serial | Model | Size | Partitions | Role | BTRFS Label |
|-----|--------|-------|------|------------|------|-------------|
| 1 | ZK208Q77 | ST2000DM008 | 1.8T | p1 (ESP) + p2 (BTRFS) | Emergency Boot/Recovery only (independent OS) | das-backup-system-mirror |
| 2 | ZXA0LMAE | ST22000NM000C (Exos) | 20T | p1 (BTRFS, whole disk) | Primary Backup -- all btrbk targets | das-backup-22tb |
| 3 | ZFL41DNY | ST2000DM008 | 1.8T | p1 (ESP) + p2 (BTRFS) | Emergency Boot/Recovery only (independent OS) | das-backup-system |
| 4 | — | — | — | — | Empty | — |
| 5 | — | — | — | — | Empty | — |
| 6 | — | — | — | — | Empty | — |

## Emergency Boot/Recovery Drives

The two 2TB drives in bays 1 and 3 are **independent standalone bootable systems** — NOT a BTRFS RAID1 pair. Each has its own ESP and its own BTRFS root filesystem with a separate UUID:

| | Bay 1 (ZK208Q77) | Bay 3 (ZFL41DNY) |
|---|---|---|
| **ESP** | `p1` — 1.5G FAT32, label `BACKUP-ESP`, UUID `6D15-0632` | `p1` — 1.5G FAT32, label `BACKUP-ESP`, UUID `6CAB-B04D` |
| **BTRFS** | `p2` — label `das-backup-system-mirror`, UUID `60b05268-7f8f-47b5-a38a-752576a1172a` | `p2` — label `das-backup-system`, UUID `7c7ae72d-09d6-4086-b249-1ac60f21b73b` |

Either drive can boot independently if the other fails. Sync between them is manual (btrbk send/receive or similar), not automatic.

## Role Summary

- **Primary Backup** (Bay 2): 22TB Exos -- all btrbk targets (NVMe, SSD, projects, audiobooks, das-storage), 4w/12m/4y retention
- **Emergency Boot/Recovery** (Bays 1, 3): Independent 2TB drives with ESP + CachyOS -- standalone bootable systems only (removed as btrbk targets 2026-04-09)

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

- **Device letters change on every reboot/reconnect** -- always identify by serial number
- LED identification: `sudo dd if=/dev/sdX of=/dev/null bs=1M count=2000 status=progress`
- 22TB Exos is CMR (conventional magnetic recording) -- no SMR write penalties
- 2TB drives: all ST2000DM008 (SMR), same batch March 2021, ~13,000 hours each
- USB topology: each bay gets an independent USB sub-device via the enclosure's bridge chip (4-1.3.x)
