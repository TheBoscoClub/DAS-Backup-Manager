# Offline Bootable Backup Plan
## Emergency Recovery for NVMe RAID-1, SSD RAID-1, and HDD RAID-1

**Date**: 2026-01-31
**Updated**: 2026-02-19
**Status**: ACTIVE — 22TB Exos primary backup drive installed, nightly btrbk running

---

## Current Storage Inventory

| Array | Devices | BTRFS Profile | Used | Total Raw |
|-------|---------|---------------|------|-----------|
| NVMe RAID-1 | 2x WD SN850X 1TB | Data/Meta/Sys RAID-1 | 377G | 1.81 TiB |
| SATA SSD RAID-1 | Samsung 860 PRO 1TB + 850 EVO mSATA 1TB | Data/Meta RAID-1 | 151G | 1.84 TiB |
| HDD RAID-1 | 2x Seagate ST24000DM001 24TB | Data RAID-0→RAID-1 (converting) | 9.03T | 43.66 TiB |

**Irreplaceable data to protect**: ~1 TiB (see Capacity Budget below)

## Backup Hardware (in TerraMaster D6-320 DAS)

- **1x Seagate ST22000NM000C** (Exos X22 22TB, 7200RPM, CMR, 256MB cache)
  - Factory recertified, installed 2026-02-19 in Bay 2
  - Serial: ZXA0LMAE, SMART clean, extended test completing 2026-02-21
  - Role: PRIMARY BACKUP — all btrbk targets, 20 TiB usable BTRFS
- **5x Seagate ST2000DM008** (Barracuda 2TB, 7200RPM, SMR, 256MB cache)
  - Same Thailand manufacturing batch: March 16, 2021
  - Age: ~4.9 years (warranty expired — was 2-year limited)
  - SMART: All healthy, within MTBF tolerance
  - Roles: 2x bootable recovery, 3x RAID0 general storage
- **Total raw capacity: ~32 TB** (22TB + 5x 2TB)

---

## Critical Risk Assessment

### Same-Batch Correlated Failure
All 6 drives share identical manufacturing conditions (same factory, same date, same component lot). This creates **correlated failure risk** — if one drive fails due to a systematic defect (bad batch of heads, platter coating issue), the others have elevated probability of similar failure within a close timeframe. This is well-documented in large-scale studies (Google, Backblaze).

**Mitigation**: The 22TB Exos enterprise drive (different manufacturer batch, CMR technology) now serves as the primary backup target, addressing this risk. The 2TB drives serve as bootable recovery systems and legacy archives — they are no longer the sole backup tier.

### SMR Technology
The ST2000DM008 uses Shingled Magnetic Recording (SMR). This means:
- Sequential writes are fine (BTRFS send streams are sequential — good match)
- Random write performance degrades severely when the drive's CMR cache fills
- Large initial backups may be slow; incremental backups will be fast
- Avoid RAID configurations that require random write patterns (no BTRFS RAID-5/6)

### Renewed Exos X14 12TB — Evaluated and Rejected
Considered adding 2x Seagate Exos X14 ST12000NM0008 12TB (renewed, ~$250 each from Amazon). Rejected because:
- Backblaze data shows Exos X14 12TB failure rates climbing to 8-9% AFR at 5+ years
- Renewed units are 6-7 years old — squarely in the elevated failure window
- Only 90-day Amazon Renewed warranty
- $500 for two aging drives with documented reliability issues is poor value for a backup role

---

## Decisions Made

| Question | Decision |
|----------|----------|
| Audiobooks | Back up **source files only** (aaxc originals, ~509G). Skip converted opus in Library/ (~450G) — re-derivable |
| VMs | **Do not back up**. All test VMs (cachyos-kwallet-dev, test-vm-cachyos, debian13, fedora43, kubuntu24.04, mxlinux-2025) are recreatable from ISOs |
| Snapper snapshots | **Do not back up**. btrbk maintains its own retention history on backup drives. Snapper snapshots are for local rollback only |
| Exos X14 12TB upgrade | **Rejected** (2026-01). Renewed units too old, high AFR. Stick with 6x ST2000DM008 only |
| Exos X22 22TB upgrade | **Accepted** (2026-02-19). Factory recertified ST22000NM000C ~$250 — excellent value for 20 TiB backup. Replaces one 2TB cold spare in DAS Bay 2 |

---

## Capacity Budget

### Back Up (irreplaceable)

| What | Size | Source |
|------|------|--------|
| NVMe: OS + /home + /root + /var/log | ~377G | `@`, `@home`, `@root`, `@log` |
| SSD: /opt + /srv | ~50-60G | `@opt`, `@srv` (no VMs, no cache) |
| HDD: ClaudeCodeProjects | ~50-100G | All project subvolumes |
| HDD: Audiobook sources (aaxc) | ~509G | `Sources/`, `Sources-GooglePlay/`, `Sources-Librivox/`, `failed-sources/`, `data/`, `scripts/` |
| **Total** | **~1 TiB** | |

### Do NOT Back Up

| What | Size | Reason |
|------|------|--------|
| Audiobooks/Library/ (opus) | ~450G | Re-derivable from source aaxc files |
| VirtualMachines | ~100G+ | Recreatable from ISOs with documented setup |
| SteamLibrary + SteamLibrary-local | ~1.4T | Re-downloadable from Steam |
| ai-models-* | ~2T+ | Re-downloadable from HuggingFace |
| ISOs | Variable | Re-downloadable |
| /var/cache | Variable | Rebuilt automatically |
| Snap packages | Variable | Reinstallable |
| Snapper snapshot history | Variable | btrbk manages its own retention on backup drives |

---

## Recommended Plan: Tiered Backup with DAS

### Hardware: TerraMaster D6-320

- **Price**: ~$300 (diskless)
- **Interface**: USB 3.2 Gen2 (10 Gbps) Type-C
- **Bays**: 6x 3.5" SATA, hot-swappable, JBOD mode (each drive appears individually)
- **Cooling**: Dual rear fans, quiet operation
- **Compatibility**: Linux plug-and-play, no drivers needed
- **Total cost**: $300 for enclosure + $0 for drives (already owned) = **$300**

### Drive Allocation Strategy (revised 2026-02-19)

With the addition of a 22TB Exos enterprise drive, all backups target a single
high-capacity drive. The two 2TB bootable recovery drives continue to receive
NVMe/SSD snapshots to keep the emergency OS current.

```
Bay 2 — 22TB Exos ST22000NM000C (ZXA0LMAE): PRIMARY BACKUP
  └── Partition 1: 20 TiB BTRFS (whole disk, no ESP)
      ├── nvme/ — @, @home, @root, @log snapshots
      ├── ssd/  — @opt, @srv snapshots
      ├── projects/ — ClaudeCodeProjects snapshots
      ├── audiobooks/ — Audiobook source snapshots
      └── storage/ — general low-I/O storage
      Retention: 4 weekly + 12 monthly + 4 yearly

Bay 6 — 2TB ST2000DM008 (ZFL41DNY): BOOTABLE RECOVERY #1
  ├── Partition 1: 1.5G ESP (FAT32, bootable clone of /boot)
  └── Partition 2: ~1998G BTRFS
      ├── nvme/ — NVMe snapshot history (btrbk secondary target)
      ├── ssd/  — SSD snapshot history (btrbk secondary target)
      └── @ @home — stable boot subvolumes (refreshed each backup run)

Bay 1 — 2TB ST2000DM008 (ZK208Q77): BOOTABLE RECOVERY #2
  ├── Partition 1: 1.5G ESP (FAT32, bootable clone of /boot)
  └── Partition 2: ~1998G BTRFS
      ├── nvme/ — NVMe snapshot history (btrbk tertiary target)
      ├── ssd/  — SSD snapshot history (btrbk tertiary target)
      └── @ @home — stable boot subvolumes (refreshed each backup run)

Bays 3,4,5 — 3x 2TB ST2000DM008: BTRFS RAID0 GENERAL STORAGE
  Bay 3: ZK208RH6, Bay 4: ZFL41DV0, Bay 5: ZK208Q7J
  └── BTRFS label: dasRaid0, mount: /dasRaid0
      ├── Data: RAID0 (~5.5 TiB striped)
      ├── Metadata: RAID1 (mirrored, survives single drive loss)
      └── @data subvolume → backed up nightly to 22TB via btrbk
```

**Rationale**: The 22TB enterprise CMR drive provides 20 TiB for deep retention
of ~1.8 TiB of source data — enough for years of backup history. The two 2TB
bootable recovery drives receive NVMe/SSD snapshots to keep the emergency
CachyOS install current. The remaining three 2TB drives (formerly cold spare
and legacy archives) are repurposed as a BTRFS RAID0 general storage array
(~5.5 TiB), backed up nightly to the 22TB.

### Why Not BTRFS RAID on the Backup Drives?

- RAID-1 across backup drives would require all drives online simultaneously — defeats offline/rotation model
- RAID-5/6 is unstable on BTRFS and terrible on SMR drives
- **Backup drives remain JBOD**: 22TB (single drive, btrbk targets), 2x 2TB bootable (independent, per-drive BTRFS)
- **RAID0 is used only for general storage** (Bays 3/4/5) — expendable data, backed up to 22TB nightly. Not used for backup targets themselves

---

## Backup Strategy: BTRFS Send/Receive with btrbk

### Software (installed)

- **btrbk** 0.32.6 — snapshot-based incremental BTRFS backup tool
- **mbuffer** — rate limiting and progress bars for send/receive streams
- **s-nail** (mailx) 14.9.25 — email delivery for nightly backup reports
- **Proton Bridge** 3.22.0 — localhost SMTP relay to Proton Mail

### How btrbk Works

1. Creates read-only snapshots of source subvolumes
2. Uses `btrfs send` to stream snapshot data to target drives
3. Incremental sends only transmit changed blocks since last backup
4. Manages retention policy on both source and target (e.g., keep 4 weekly + 2 monthly)
5. Uses mbuffer for buffered transfers with progress monitoring

### No Snapper Snapshot Transfer

Snapper snapshots on the live system are **not transferred** to backup drives. Rationale:
- Snapper is for local rollback ("undo the last 2 hours")
- btrbk creates and manages its own snapshot retention on the backup drives
- Transferring snapper history would multiply backup time and space for no disaster recovery benefit
- The current live state is what matters for recovery — btrbk's own retention provides historical depth

### Workflow (revised 2026-02-19)

The backup architecture uses three target tiers:

1. **22TB Exos (Bay 2)** — receives ALL subvolumes (NVMe, SSD, HDD projects, audiobooks)
2. **2TB Bootable Recovery #1 (Bay 6)** — receives NVMe + SSD snapshots only (keeps emergency OS current)
3. **2TB Bootable Recovery #2 (Bay 1)** — receives NVMe + SSD snapshots only (mirror of #1)

#### Nightly Automated Backup
The `das-backup.timer` systemd unit runs nightly at 03:00:
1. `backup-run.sh` detects DAS drives by serial number
2. Mounts source top-level volumes (NVMe, SSD, HDD) and all three targets
3. Records pre-backup disk usage for throughput measurement
4. Runs `btrbk run` — creates snapshots and sends incremental deltas to all targets
5. Records post-backup usage, logs per-target throughput (data written + MB/s)
6. Updates stable boot subvolumes (@, @home) on bootable recovery drives
7. Syncs ESP to both 2TB bootable drives (rsync of /boot)
8. Records growth data point for trend analysis
9. Generates and emails a comprehensive backup report (see Email Reports below)
10. Unmounts all volumes and cleans up

#### Manual Backup
```bash
sudo /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/scripts/backup-run.sh           # incremental
sudo /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/scripts/backup-run.sh --dryrun  # preview
sudo /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/scripts/backup-run.sh --full    # force full
```

#### Schedule
- **Nightly**: Automated via systemd timer (03:00)
- **Monthly**: Run SMART checks on all DAS drives (`backup-verify.sh`)
- **Quarterly**: Test boot from DAS recovery drive, verify restore capability
- **As needed**: Check SMART extended test results on 22TB

### Bootable Recovery Drives (Bays 1 and 6)

Two 2TB drives serve as emergency bootable CachyOS systems:

```
Each 2TB bootable drive:
├── Partition 1: 1.5G ESP (FAT32) — synced clone of /boot
└── Partition 2: ~1998G BTRFS
    ├── nvme/ — btrbk NVMe snapshot history
    ├── ssd/  — btrbk SSD snapshot history
    └── @ @home — stable boot subvolumes (refreshed each backup run)
```

**Recovery procedure**:
1. Connect DAS via USB
2. Select "Emergency Backup" or "Emergency Backup Mirror" in UEFI boot menu
3. System boots from the 2TB drive's ESP → systemd-boot
4. Root mounts from the 2TB drive's BTRFS (@ subvolume)
5. From there, repair or reinstall to production NVMe
6. Full btrbk snapshot history available on the 22TB for data recovery

**Note**: The 22TB primary backup drive has NO ESP and is NOT bootable — it stores
btrbk snapshots only. The bootable recovery capability lives on the two 2TB drives.

**Caveat**: The backup's systemd-boot entries must have their `root=` parameters pointing to the backup BTRFS UUID, not the NVMe UUID. `backup-run.sh` maintains separate loader entries on each backup ESP automatically.

### Email Reports (added 2026-02-19)

After each backup (nightly or manual), `backup-run.sh` generates and emails a
comprehensive report to `gjbr@pm.me` via Proton Bridge SMTP.

**Report contents**:
- **Backup Operations** — btrbk success/fail + duration, boot subvolume update status, ESP sync status
- **Throughput** — per-target data written and transfer rate (MB/s)
- **Disk Capacity** — used/available/percentage for all backup targets
- **Growth Analysis** — today's growth, 7-day avg, 30-day avg, capacity runway projection (years until full)
- **SMART Status** — health, temperature, power-on hours for each DAS drive
- **Latest Snapshots** — most recent btrbk snapshot per subvolume

**Email delivery chain**: `backup-run.sh` → `mailx` (s-nail) → Proton Bridge (127.0.0.1:1025) → Proton Mail

**Configuration files**:
- `/etc/das-backup-email.conf` — SMTP credentials (mode 600, root-only)
- `/var/lib/das-backup/growth.log` — append-only growth data for trend analysis
- `/var/lib/das-backup/last-report.txt` — most recent report (always saved regardless of email success)

**Subject line format**: `[DAS Backup] cachyos-bosco — SUCCESS — 2026-02-20 03:07`

---

## Cost Analysis

| Item | Cost | Notes |
|------|------|-------|
| TerraMaster D6-320 | $300 | 6-bay USB 3.2 Gen2 DAS enclosure |
| 5x ST2000DM008 2TB | $0 | Already owned (same-batch March 2021) |
| 1x ST22000NM000C 22TB Exos | ~$250 | Factory recertified, added 2026-02-19 |
| USB-C cable | $0 | Included with D6-320 |
| **Total** | **~$550** | |

---

## Implementation Phases (all complete)

### Phase A: Acquire DAS — DONE (2026-02-01)
- Purchased TerraMaster D6-320
- Verified USB 3.2 Gen2 connectivity

### Phase B: Prepare Drives — DONE (2026-02-04)
- SMART extended tests passed on all 6x ST2000DM008
- Partitioned two bootable drives: p1 1.5G ESP + p2 BTRFS
- Formatted data drives as whole-disk BTRFS

### Phase C: Configure btrbk — DONE (2026-02-04, revised 2026-02-19)
- `/etc/btrbk/btrbk.conf` v2.0.0 — triple-target architecture
- Source: NVMe (@, @home, @root, @log), SSD (@opt, @srv), HDD (projects, audiobooks)
- 22TB primary target: all subvolumes, 4w 12m 4y retention
- 2TB secondary/tertiary targets: NVMe + SSD only (keeps recovery OS current)

### Phase D: Initial Full Backup — DONE (2026-02-04 original, 2026-02-19 for 22TB)
- Original 2TB targets: initial sends completed 2026-02-04
- 22TB Exos: initial full send started 2026-02-19 (~1.8 TiB transfer)
- Boot subvolumes created on both 2TB recovery drives
- ESP synced to both bootable drives
- Boot from DAS tested and verified

### Phase E: Automate — DONE (2026-02-04, enhanced 2026-02-19)
- `das-backup.service` + `das-backup.timer` — nightly at 03:00
- `backup-verify.sh` v2.0.0 — SMART check, btrbk status, disk usage
- `backup-run.sh` v3.1.0 — triple-target, throughput logging, email reports
- `/etc/das-backup-email.conf` — SMTP config for Proton Bridge
- `/var/lib/das-backup/` — growth tracking and report archive

### Phase F: 22TB Exos Integration — DONE (2026-02-19)
- Replaced Bay 2 cold spare (ZFL416F6) with 22TB Exos (ZXA0LMAE)
- Partitioned as single whole-disk BTRFS (no ESP — not a bootable drive)
- SMART extended self-test started (completes ~2026-02-21 14:26)
- btrbk reconfigured for triple-target architecture
- Legacy 2TB data drives (Bays 4, 5) retained read-only until 22TB proven

### Phase H: RAID0 General Storage — DONE (2026-02-19)
- Repurposed Bays 3/4/5 (formerly cold spare + legacy archives) as BTRFS RAID0
- 3x 2TB ST2000DM008: data RAID0 (~5.5 TiB), metadata RAID1
- Label: dasRaid0, mount: /dasRaid0, subvolume: @data
- Added to btrbk config: snapshots to /mnt/backup-22tb/das-storage
- fstab entry with nofail (DAS may be off), nossd, autodefrag, compress=zstd:3

### Phase G: Ongoing
- **Nightly**: Automated incremental backup via systemd timer
- **Monthly**: SMART checks on all DAS drives
- **Quarterly**: Test boot from DAS recovery drive, verify restore capability
- **2026-02-21**: Check 22TB SMART extended test results

---

## References

- [TerraMaster D6-320 — Amazon](https://www.amazon.com/TERRAMASTER-D6-320-External-Drive-Enclosure/dp/B0BZHSK29B)
- [TerraMaster D6-320 — TechRadar Review](https://www.techradar.com/pro/terramaster-d6-320-6-bay-review)
- [TerraMaster D6-320 — The Gadgeteer Review](https://the-gadgeteer.com/2024/01/16/terramaster-d6-320-6-bay-external-hard-disk-enclosure-review-wonderfully-boring/)
- [btrbk — GitHub](https://github.com/digint/btrbk)
- [BTRFS Incremental Backup — Fedora Magazine](https://fedoramagazine.org/btrfs-snapshots-backup-incremental/)
- [Seagate ST2000DM008 Product Manual (PDF)](https://www.seagate.com/files/www-content/product-content/barracuda-fam/barracuda-new/en-us/docs/100817550m.pdf)
- [Seagate Exos X22 (ST22000NM000C) Product Page](https://www.seagate.com/products/enterprise-drives/exos-x/x22/)
- [Seagate HDD Reliability & MTBF](https://www.seagate.com/support/kb/hard-disk-drive-reliability-and-mtbf-afr-174791en/)
- [Backblaze Drive Stats Q2 2025](https://www.backblaze.com/blog/backblaze-drive-stats-for-q2-2025/) — Exos X14 failure rate data
- [Backblaze Drive Stats 2024](https://www.backblaze.com/blog/backblaze-drive-stats-for-2024/) — Exos X14 failure rate data
