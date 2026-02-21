# TerraMaster D6-320 Bay Mapping

**Date mapped**: 2026-02-04
**Updated**: 2026-02-19 (22TB Exos in Bay 2; Bays 3/4/5 repurposed as BTRFS RAID0)
**Method**: I/O activity LED identification + serial number verification

## Physical Bay Layout

```
┌───────────────────────────────────────────────────┐
│ TerraMaster D6-320 (front view)                   │
├──────────────┬──────────────┬─────────────────────┤
│    Bay 1     │    Bay 2     │    Bay 3             │
│   ZK208Q77   │   ZXA0LMAE   │   ZK208RH6           │
│   2TB SMR    │ ★ 22TB CMR   │   2TB SMR             │
│  Bootable    │  PRIMARY     │  ═══ RAID0 ═══       │
│  Recovery    │  BACKUP      │  General Storage     │
├──────────────┼──────────────┼─────────────────────┤
│    Bay 4     │    Bay 5     │    Bay 6             │
│   ZFL41DV0   │   ZK208Q7J   │   ZFL41DNY           │
│   2TB SMR    │   2TB SMR    │   2TB SMR             │
│ ═══ RAID0 ══ │ ═══ RAID0 ══ │  Bootable            │
│ Gen. Storage │ Gen. Storage │  Recovery            │
└──────────────┴──────────────┴─────────────────────┘
```

## Drive Details

| Bay | Serial | Model | Size | Partitions | Role | BTRFS Label |
|-----|--------|-------|------|------------|------|-------------|
| 1 | ZK208Q77 | ST2000DM008 | 1.8T | p1 (ESP) + p2 (BTRFS) | Bootable Recovery + btrbk NVMe/SSD target | das-backup-system-mirror |
| 2 | ZXA0LMAE | ST22000NM000C (Exos) | 20T | p1 (BTRFS, whole disk) | Primary Backup — all btrbk targets | das-backup-22tb |
| 3 | ZK208RH6 | ST2000DM008 | 1.8T | whole disk (BTRFS RAID0) | RAID0 General Storage (member 1/3) | dasRaid0 |
| 4 | ZFL41DV0 | ST2000DM008 | 1.8T | whole disk (BTRFS RAID0) | RAID0 General Storage (member 2/3) | dasRaid0 |
| 5 | ZK208Q7J | ST2000DM008 | 1.8T | whole disk (BTRFS RAID0) | RAID0 General Storage (member 3/3) | dasRaid0 |
| 6 | ZFL41DNY | ST2000DM008 | 1.8T | p1 (ESP) + p2 (BTRFS) | Bootable Recovery + btrbk NVMe/SSD target | das-backup-system |

## RAID0 General Storage Array

- **Label**: dasRaid0
- **UUID**: d29fdda7-a1e5-4640-996e-2b78569cb65d
- **Mount**: /dasRaid0
- **Data profile**: RAID0 (striped, ~5.5 TiB usable)
- **Metadata profile**: RAID1 (mirrored across 2 of 3 devices — survives single drive loss for file listing)
- **Subvolume**: @data (backed up nightly to 22TB via btrbk)
- **Use case**: Incidental/replaceable data. SMR drives are fine for large sequential writes but poor for small random I/O.
- **Fault tolerance**: NONE for data (any single drive loss = array offline). Data backed up to 22TB nightly.

## Role Summary

- **Primary Backup** (Bay 2): 22TB Exos — all btrbk targets (NVMe, SSD, projects, audiobooks, DAS storage), deep retention (4w 12m 4y)
- **Bootable Recovery** (Bays 1, 6): 2TB drives with ESP + CachyOS — also receive btrbk NVMe/SSD snapshots to keep recovery OS current
- **RAID0 General Storage** (Bays 3, 4, 5): ~5.5 TiB striped array for expendable data, backed up to 22TB nightly

## Removed Drive

| Serial | Model | Size | Former Role | Status |
|--------|-------|------|-------------|--------|
| ZFL416F6 | ST2000DM008 | 1.8T | Bay 2 Cold Spare | Removed 2026-02-19, stored offline |

## Notes

- **Device letters change on every reboot/reconnect** — always identify by serial number
- LED identification: `sudo dd if=/dev/sdX of=/dev/null bs=1M count=2000 status=progress`
- 22TB Exos is CMR (conventional magnetic recording) — no SMR write penalties
- 22TB SMART extended self-test started 2026-02-19, completes ~2026-02-21 14:26
- 2TB drives: all ST2000DM008 (SMR), same batch March 2021, ~13,000 hours each
- RAID0 array: all 3 drives on USB 3.2 Gen2 via DAS — all must be present to mount
