# Planning Your DAS Backup

This guide covers capacity estimation, drive selection, retention planning, and DAS enclosure requirements for a BTRFS backup system using btrbk.

---

## Core Concepts

### Why DAS for Backups?

Direct-Attached Storage (DAS) provides:

- **Offline capability** -- unplug the DAS and your backups are air-gapped from ransomware, accidental deletion, and network-based attacks
- **No network dependency** -- transfers happen over USB/Thunderbolt/eSATA at local bus speeds
- **Simple JBOD** -- each drive appears independently to the OS, no hardware RAID controller to fail
- **Portability** -- take your backups offsite by carrying the enclosure

### BTRFS Send/Receive with btrbk

btrbk orchestrates BTRFS snapshot-based backups:

1. Creates read-only snapshots of source subvolumes
2. Uses `btrfs send` to stream snapshot data to target drives
3. Incremental sends only transmit changed blocks since the last backup
4. Manages retention policy on both source and target (e.g., keep 4 weekly + 12 monthly)
5. Uses mbuffer for buffered transfers with progress monitoring

**Key advantage**: Incremental sends are extremely efficient. After the initial full send, nightly backups transfer only changed data -- typically completing in minutes.

### No Snapper Snapshot Transfer

If you use Snapper for local rollback, those snapshots are **not** transferred to backup drives:

- Snapper is for local undo ("revert the last 2 hours")
- btrbk creates and manages its own snapshot retention on the backup drives
- Transferring Snapper history would multiply backup time and space for no disaster recovery benefit
- The current live state is what matters for recovery -- btrbk provides historical depth on the targets

### Bootable Recovery Drives

You can designate one or more DAS drives as bootable recovery systems:

- Partition with an ESP (EFI System Partition) + BTRFS root
- The ESP contains a bootloader (systemd-boot, GRUB, etc.) with kernel and initramfs
- BTRFS partition holds a bootable OS installation plus btrbk snapshot history
- To recover: plug in DAS, select it from UEFI boot menu, boot into a working system
- From the recovery OS, repair or reinstall to production drives

**ESP mirroring**: If you have multiple bootable recovery drives, `btrdasd setup` can configure package manager hooks to automatically sync the ESP across all of them whenever the kernel or bootloader updates.

### Tiered Backup Architecture

A tiered approach separates concerns:

- **Primary backup target** -- large-capacity drive receiving ALL subvolumes with deep retention
- **Recovery drives** (optional) -- smaller drives that receive only OS-critical subvolumes (boot, root, home) and maintain a bootable system
- **General storage** (optional) -- expendable data on additional drives, backed up to the primary target

This means your most important data has the deepest retention on the largest drive, while recovery drives stay small and focused on getting you booted quickly.

### Why Not BTRFS RAID on Backup Drives?

- RAID-1 across backup drives requires all drives online simultaneously -- defeats the offline/rotation model
- RAID-5/6 is unstable on BTRFS and performs poorly on SMR drives
- **Backup drives should remain JBOD**: each drive is independent with its own BTRFS filesystem
- RAID0 is acceptable only for expendable general storage (not backup targets)

---

## Deciding What to Back Up

### Back Up (irreplaceable)

- OS root, home directory, system configuration
- Project source code and data
- Application data that cannot be recreated
- Original media files (before conversion/transcoding)

### Do NOT Back Up (re-derivable or re-downloadable)

- Converted/transcoded media (recreatable from originals)
- Virtual machines (recreatable from ISOs and documented setup)
- Game libraries (re-downloadable from storefronts)
- AI models (re-downloadable from model hubs)
- Package caches (rebuilt automatically)
- ISO images (re-downloadable)
- Snapper snapshot history (btrbk manages its own retention)

---

## DAS Backup Planning Worksheet

### 1. IRREPLACEABLE DATA SIZE

Inventory your BTRFS subvolumes and estimate sizes:

| Subvolume | Mount Point | Size | Back Up? |
|-----------|-------------|------|----------|
| `<subvol>` | `<mount>` | `___` GB/TB | Yes / No |
| `<subvol>` | `<mount>` | `___` GB/TB | Yes / No |
| ... | ... | ... | ... |

**Total data to back up**: _____ GB/TB

### 2. RETENTION DEPTH

How much history do you want to keep?

| Retention Period | Count | Estimated Multiplier |
|-----------------|-------|---------------------|
| Weekly snapshots | _____ | 1.1x -- 1.3x (low churn data) |
| Monthly snapshots | _____ | 1.5x -- 2.0x |
| Yearly snapshots | _____ | 2.0x -- 3.0x |

**Estimated space**: data_size x retention_factor = _____

*Note*: btrbk uses incremental snapshots. The multiplier depends on your data's rate of change. Code repositories with frequent commits need more space than static media archives.

### 3. TARGET CAPACITY

| Target | Minimum Capacity |
|--------|-----------------|
| Primary backup drive | >= (data x retention_factor) |
| Recovery drives (optional) | >= OS data size each |

### 4. DAS ENCLOSURE

| Requirement | Your Choice |
|-------------|-------------|
| Bays needed | _____ |
| Interface | USB 3.x / eSATA / Thunderbolt / 10GbE |
| Mode | **JBOD required** (drives must appear individually to the OS) |
| Hot-swap | Recommended but not required |
| Cooling | Verify adequate for your drive count |

**Important**: The enclosure **must** operate in JBOD mode. Hardware RAID enclosures that present a single virtual disk are incompatible with per-drive BTRFS backup.

### 5. DRIVE SELECTION

| Consideration | Your Choice |
|---------------|-------------|
| Technology | HDD / SSD / NVMe |
| Recording method (HDD only) | CMR preferred for random I/O; SMR acceptable for sequential workloads |
| Capacity per drive | _____ |
| Quantity | _____ |
| Total raw capacity | _____ |

**Drive technology notes**:
- **HDD (CMR)**: Best cost-per-TB. Conventional Magnetic Recording handles all workloads well.
- **HDD (SMR)**: Cheaper but sequential-write-only. Acceptable for BTRFS send streams (which are sequential) but avoid for RAID configurations requiring random writes.
- **SSD**: Faster, silent, shock-resistant. Higher cost-per-TB. No SMR concerns.
- **NVMe in USB enclosure**: Fastest option. Highest cost-per-TB.

### 6. BUDGET

| Item | Cost |
|------|------|
| DAS enclosure | $_____ |
| Drives | $_____ |
| Cables (if not included) | $_____ |
| **Total** | **$_____** |

---

## Backup Workflow

### Nightly Automated Backup

The `das-backup.timer` systemd unit (or cron job on sysvinit/OpenRC) runs nightly:

1. `backup-run.sh` detects DAS drives by serial number
2. Mounts source top-level volumes and all target drives
3. Records pre-backup disk usage for throughput measurement
4. Runs `btrbk run` -- creates snapshots and sends incremental deltas to all targets
5. Records post-backup usage, logs per-target throughput
6. Updates stable boot subvolumes on bootable recovery drives (if configured)
7. Syncs ESP to bootable drives (if configured)
8. Records growth data point for trend analysis
9. Generates and emails a backup report (if configured)
10. Unmounts all volumes and cleans up

### Manual Backup

```bash
sudo backup-run.sh                # incremental
sudo backup-run.sh --dryrun       # preview
sudo backup-run.sh --full         # force full send
```

### Recommended Schedule

- **Nightly**: Automated incremental backup via systemd timer / cron
- **Monthly**: Run SMART checks on all DAS drives (`backup-verify.sh`)
- **Quarterly**: Test boot from DAS recovery drive (if applicable), verify restore capability
- **As needed**: Check SMART extended test results on large drives

### Email Reports

After each backup, the report includes:

- **Backup Operations** -- btrbk success/fail + duration, boot subvolume update status, ESP sync status
- **Throughput** -- per-target data written and transfer rate
- **Disk Capacity** -- used/available/percentage for all backup targets
- **Growth Analysis** -- daily growth, 7-day average, 30-day average, capacity runway projection
- **SMART Status** -- health, temperature, power-on hours for each DAS drive
- **Latest Snapshots** -- most recent btrbk snapshot per subvolume

---

## Risk Considerations

### Same-Batch Correlated Failure

If all your DAS drives come from the same manufacturing batch, they share identical manufacturing conditions. This creates **correlated failure risk** -- if one drive fails due to a systematic defect, others from the same batch have elevated probability of similar failure. This is well-documented in large-scale studies (Google, Backblaze).

**Mitigation**: Use drives from different batches or manufacturers for different tiers (e.g., enterprise CMR drive for primary backup, consumer drives for recovery).

### SMR Write Performance

Shingled Magnetic Recording (SMR) drives have poor random write performance once their CMR cache fills. For DAS backup use:

- **Sequential writes are fine** -- BTRFS send streams are sequential, which is a good match
- **Avoid BTRFS RAID-5/6** on SMR drives -- these require random write patterns
- **Large initial backups may be slow** on SMR; incremental backups will be fast

---

## Reference Example: Author's Setup

> **Disclaimer**: This documents the author's personal setup as one example. No endorsement of any manufacturer, enclosure model, or drive architecture. Your requirements will differ.

### Hardware

- **Enclosure**: TerraMaster D6-320, 6-bay USB 3.2 Gen2 JBOD, ~$300
- **Primary backup**: 1x Seagate Exos X22 22TB (ST22000NM000C), CMR, factory recertified ~$250
- **Recovery + storage**: 5x Seagate Barracuda 2TB (ST2000DM008), SMR, same batch March 2021
- **Total raw capacity**: ~32 TB (22TB + 5x 2TB)
- **Total cost**: ~$550

### Drive Allocation

```
Bay 2 -- 22TB Exos: PRIMARY BACKUP
  All btrbk targets (NVMe, SSD, HDD projects, audiobooks, DAS storage)
  Retention: 4 weekly + 12 monthly + 4 yearly

Bay 6 -- 2TB Barracuda: BOOTABLE RECOVERY #1
  ESP + bootable OS + btrbk NVMe/SSD snapshots

Bay 1 -- 2TB Barracuda: BOOTABLE RECOVERY #2
  ESP + bootable OS + btrbk NVMe/SSD snapshots (mirror of #1)

Bays 3,4,5 -- 3x 2TB Barracuda: BTRFS RAID0 GENERAL STORAGE
  ~5.5 TiB striped array for expendable data, backed up to 22TB nightly
```

### Capacity Budget

| Data Category | Size | Source |
|---------------|------|--------|
| NVMe: OS + /home + /root + /var/log | ~377G | `@`, `@home`, `@root`, `@log` |
| SSD: /opt + /srv | ~50-60G | `@opt`, `@srv` |
| HDD: Projects | ~50-100G | Project subvolumes |
| HDD: Audiobook sources | ~509G | Original source files only |
| **Total irreplaceable** | **~1 TiB** | |

### What Is NOT Backed Up

| Category | Size | Reason |
|----------|------|--------|
| Converted audiobooks | ~450G | Re-derivable from sources |
| Virtual machines | ~100G+ | Recreatable from ISOs |
| Game libraries | ~1.4T | Re-downloadable |
| AI models | ~2T+ | Re-downloadable |
| ISOs, caches, Snapper | Variable | Re-downloadable or auto-rebuilt |

### Cost Analysis

| Item | Cost |
|------|------|
| TerraMaster D6-320 | $300 |
| 5x ST2000DM008 2TB (already owned) | $0 |
| 1x ST22000NM000C 22TB Exos (recertified) | ~$250 |
| **Total** | **~$550** |

### Evaluated and Rejected: Renewed Exos X14 12TB

Considered 2x Seagate Exos X14 12TB renewed (~$250 each). Rejected because:
- Backblaze data shows Exos X14 12TB failure rates climbing to 8-9% AFR at 5+ years
- Renewed units are 6-7 years old -- in the elevated failure window
- Only 90-day warranty
- Poor value for a backup role

---

## References

- [btrbk -- GitHub](https://github.com/digint/btrbk)
- [BTRFS Incremental Backup -- Fedora Magazine](https://fedoramagazine.org/btrfs-snapshots-backup-incremental/)
- [Backblaze Drive Stats](https://www.backblaze.com/cloud-storage/resources/hard-drive-test-data) -- Annual failure rate data by model
- [Seagate HDD Reliability & MTBF](https://www.seagate.com/support/kb/hard-disk-drive-reliability-and-mtbf-afr-174791en/)
