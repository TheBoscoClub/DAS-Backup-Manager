# Planning Your DAS Backup

This guide covers capacity estimation, drive selection, retention planning, and DAS enclosure requirements for a BTRFS backup system using btrbk.

> The planning sections (Core Concepts through the worksheet, and Risk
> Considerations) are general guidance, not a description of this project's
> behaviour. **Backup Workflow** describes what the installed system does. Which
> subvolume goes to which target is in `/etc/btrbk/btrbk.conf`, generated from
> `/etc/das-backup/config.toml`.

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

**ESP mirroring**: `btrdasd setup` does **not** provide this — it never mirrors or syncs an
ESP/boot partition, because a mirror hook that discovers ESPs by type can overwrite the
independent ESPs of recovery drives. If you want multiple
bootable recovery drives to stay in sync, that is a manual or host-level concern entirely
outside this project's scope, independent of any `[[target]]` role here.

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

1. `backup-run.sh` takes its locks (waiting if a scrub holds the maintenance lock) and checks that the DAS is connected
2. Mounts the source top-level volumes and verifies each is the expected filesystem
3. Syncs subvolumes with the config (`btrdasd subvol sync`): adopts new subvolumes, retires vanished ones, regenerates `btrbk.conf` when it differs
4. Mounts the targets by filesystem UUID (`mount_uuid`), else by drive serial, and verifies each is the expected filesystem before anything writes to it
5. Records pre-backup disk usage for throughput measurement
6. Runs `btrbk run` -- creates snapshots and sends incremental deltas to the targets each source names
7. Deletes the backups of retired subvolumes that are past their retention window (`btrdasd subvol expire`)
8. Records post-backup usage, logs throughput and the negotiated USB link speed
9. Creates missing boot subvolumes on non-mirror targets, and archives and recreates them on `--full` runs (skips `role=mirror` targets)
10. Indexes the new snapshots (`btrdasd walk`) and records a growth data point
11. Prunes expired boot subvolume archives past `[boot].archive_retention_days`
12. Reads the report data while the targets are mounted, then unmounts them (each unmount retried 5 times, 2 s apart)
13. Writes the report to `last_report`, emails it (if configured), and records the run in the database

There is no ESP sync step; ESP/boot partition mirroring is outside this project.

### Manual Backup

`backup-run.sh` is not on `$PATH` — it is installed flat at `${prefix}/lib/das-backup/`
(`/usr/lib/das-backup/backup-run.sh` with the default `install_prefix = /usr`):

```bash
sudo /usr/lib/das-backup/backup-run.sh                # incremental
sudo /usr/lib/das-backup/backup-run.sh --dryrun       # preview
sudo /usr/lib/das-backup/backup-run.sh --full         # also archive + recreate boot subvolumes
```

### Recommended Schedule

- **Nightly**: Automated incremental backup via systemd timer / cron (`das-backup.timer`, 03:00; `das-backup-full.timer` adds the boot-subvolume step on Sundays)
- **Weekly (automatic)**: Subvolume drift check (`das-backup-doctor.timer`, emails only when it finds drift)
- **Monthly (automatic)**: BTRFS scrub of every backup target (`das-scrub.timer`, `btrdasd scrub run`)
- **Monthly**: Run SMART checks on all DAS drives (`backup-verify.sh`, run by hand)
- **Quarterly**: Test boot from DAS recovery drive (if applicable), verify restore capability
- **As needed**: Check SMART extended test results on large drives

### Email Reports

After each backup, the report includes:

- **Backup Operations** -- maintenance-lock wait, btrbk success/fail + duration, boot subvolume archive/cleanup status, unmount status, content indexer, subvolume sync, retired-subvolume expiry
- **Run History** -- only when the run could not be recorded: it is missing from the backup history, and the report is sent again with this section and `FAILURES DETECTED`
- **Subvolume Sync / Retired Subvolumes** -- what was adopted, retired, skipped or expired (when there is anything to say)
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

## References

- [btrbk -- GitHub](https://github.com/digint/btrbk)
- [BTRFS Incremental Backup -- Fedora Magazine](https://fedoramagazine.org/btrfs-snapshots-backup-incremental/)
- [Backblaze Drive Stats](https://www.backblaze.com/cloud-storage/resources/hard-drive-test-data) -- Annual failure rate data by model
- [Seagate HDD Reliability & MTBF](https://www.seagate.com/support/kb/hard-disk-drive-reliability-and-mtbf-afr-174791en/)
