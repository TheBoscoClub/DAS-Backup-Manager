# Storage Architecture & Recovery Guide

This guide covers how to document your storage topology, understand BTRFS RAID concepts, detect failures, and recover from drive loss.

---

## Table of Contents

1. [Documenting Your Storage Topology](#1-documenting-your-storage-topology)
2. [BTRFS RAID Concepts](#2-btrfs-raid-concepts)
3. [Failure Detection](#3-failure-detection)
4. [Immediate Response Checklist](#4-immediate-response-checklist)
5. [Recovery Procedures](#5-recovery-procedures)
6. [Post-Replacement Verification](#6-post-replacement-verification)
7. [Quick Reference Templates](#7-quick-reference-templates)

---

## 1. Documenting Your Storage Topology

Before a failure happens, document your storage layout. When you are troubleshooting at 2 AM with a degraded array, you will be grateful for clear records of UUIDs, serials, and partition layouts.

### Device Inventory Template

| Device | Model | Serial | Size | Role | BTRFS devid |
|--------|-------|--------|------|------|-------------|
| `<device>` | `<model>` | `<serial>` | `<size>` | `<role>` | `<devid>` |
| `<device>` | `<model>` | `<serial>` | `<size>` | `<role>` | `<devid>` |

Populate this with:
```bash
# List all block devices
lsblk -o NAME,SIZE,MODEL,SERIAL,FSTYPE,UUID

# Detailed SMART info per drive
sudo smartctl -i /dev/<device>

# BTRFS device IDs
sudo btrfs filesystem show
```

### BTRFS Filesystem UUID Template

| Array | UUID | Label | Devices |
|-------|------|-------|---------|
| `<array-name>` | `<uuid>` | `<label>` | `<dev1>`, `<dev2>` |

### Subvolume Map Template

| Subvolume | Mount Point | Array | Mount Options |
|-----------|-------------|-------|---------------|
| `<subvol>` | `<mount>` | `<array>` | `<options>` |

### ESP (Boot Partition) Layout Template

If you use ESP mirroring:

| Partition | UUID | Mount Point | Role |
|-----------|------|-------------|------|
| `<device-partition>` | `<uuid>` | `/boot` | Primary ESP |
| `<device-partition>` | `<uuid>` | `/mnt/esp-backup` | Backup ESP |

### Partition Layout Template

For each drive with multiple partitions, document the layout:

```
Partition   Size    Type              Purpose
-----------------------------------------------
p1          <size>  EFI System        ESP / bootloader
p2          <size>  Linux Swap        Swap partition
p3          <size>  Linux Filesystem  BTRFS RAID member
```

---

## 2. BTRFS RAID Concepts

### Data Profiles

BTRFS manages RAID at the filesystem level (not the block device level). Each filesystem has independent profiles for data, metadata, and system chunks:

| Profile | Redundancy | Min Devices | Space Efficiency | Notes |
|---------|-----------|-------------|-----------------|-------|
| single | None | 1 | 100% | No redundancy |
| RAID-0 | None | 2+ | N x capacity | Striped, any device loss = total loss |
| RAID-1 | 1 copy | 2+ | 50% | Mirrored across 2 devices |
| RAID-1C3 | 2 copies | 3+ | 33% | 3 copies of each block |
| RAID-10 | 1 copy | 4+ | 50% | Striped mirrors |
| RAID-5 | 1 parity | 3+ | (N-1)/N | **Not recommended** -- known stability issues |
| RAID-6 | 2 parity | 4+ | (N-2)/N | **Not recommended** -- known stability issues |

**Recommendation**: Use RAID-1 for critical data. Avoid RAID-5/6 on BTRFS -- they have known write-hole bugs and are not considered production-ready.

### Degraded Mounting

When one device in a RAID-1 array fails or is missing, BTRFS can mount in degraded mode:

```bash
sudo mount -o degraded /dev/<surviving-device> /mountpoint
# Or via UUID:
sudo mount -o degraded UUID=<your-uuid> /mountpoint
```

For root filesystems, add `degraded` to kernel boot options:
```
rootflags=subvol=/@,degraded
```

**Important**: Degraded mode provides no redundancy. Replace the failed device as soon as possible.

### Device Replace

BTRFS can replace a failed device with a new one while the filesystem is online:

```bash
# Find the missing device's devid
sudo btrfs filesystem show /mountpoint

# Replace it (runs in background by default)
sudo btrfs replace start <devid> /dev/<new-device> /mountpoint

# Run in foreground for monitoring
sudo btrfs replace start <devid> /dev/<new-device> /mountpoint -B

# Check progress
sudo btrfs replace status /mountpoint
```

### Balance (Profile Conversion)

Convert data profile from one RAID level to another:

```bash
# Convert data chunks from RAID-0 to RAID-1
sudo btrfs balance start -dconvert=raid1 /mountpoint

# Convert metadata to RAID-1
sudo btrfs balance start -mconvert=raid1 /mountpoint

# Check progress
sudo btrfs balance status /mountpoint
```

**Warning**: If a balance from RAID-0 to RAID-1 is interrupted (by failure or reboot), some chunks will be RAID-0 and some RAID-1. The RAID-0 chunks have no redundancy. Check with:
```bash
sudo btrfs filesystem df /mountpoint
# Look for both "Data, RAID0" and "Data, RAID1" lines
```

### Scrub (Integrity Verification)

Scrub reads every block on all devices and verifies checksums. If it finds corruption on one device, it auto-repairs from the mirror:

```bash
# Start a scrub (runs in background)
sudo btrfs scrub start /mountpoint

# Run in foreground
sudo btrfs scrub start -B /mountpoint

# Check status
sudo btrfs scrub status /mountpoint
```

Run scrubs periodically (monthly is a common schedule) to catch silent data corruption before it spreads.

---

## 3. Failure Detection

### 3a. SMART Monitoring

```bash
# NVMe drives
sudo smartctl -a /dev/<nvme-device>

# SATA drives (SSD or HDD)
sudo smartctl -a /dev/<sata-device>
```

**Key SMART attributes to watch**:
- **NVMe**: `Percentage Used`, `Media and Data Integrity Errors`, `Error Information Log Entries`
- **SATA SSD**: `Reallocated_Sector_Ct`, `Wear_Leveling_Count`, `Runtime_Bad_Block`
- **SATA HDD**: `Reallocated_Sector_Ct`, `Current_Pending_Sector`, `Offline_Uncorrectable`, `UDMA_CRC_Error_Count`

### 3b. BTRFS Device Stats

```bash
# Check error counters -- ANY non-zero value means a problem
sudo btrfs device stats /mountpoint
```

Expected healthy output:
```
[/dev/<device>].write_io_errs    0
[/dev/<device>].read_io_errs     0
[/dev/<device>].flush_io_errs    0
[/dev/<device>].corruption_errs  0
[/dev/<device>].generation_errs  0
```

**Interpretation**:
- `write_io_errs > 0`: Drive cannot write -- likely failing hardware
- `read_io_errs > 0`: Drive cannot read -- data may be corrupt; BTRFS will use mirror if available
- `corruption_errs > 0`: Checksum mismatch -- BTRFS detected bit rot, auto-repaired from mirror
- `generation_errs > 0`: Metadata generation mismatch -- filesystem inconsistency

**Reset counters after replacement** (to clear stale stats):
```bash
sudo btrfs device stats --reset /mountpoint
```

### 3c. dmesg Patterns

```bash
# Look for I/O errors
sudo dmesg | grep -iE 'i/o error|medium error|blk_update_request|btrfs.*error|ata.*failed'
```

Common failure patterns:
- `blk_update_request: I/O error, dev <dev>, sector NNNN` -- HDD/SSD sector failure
- `ata3: COMRESET failed` -- SATA link failure
- `BTRFS error (device <dev>): bdev /dev/<dev> errs` -- BTRFS detected device error
- `nvme nvme0: I/O Cmd(0x02) error` -- NVMe read failure

### 3d. Degraded Mount Detection

```bash
# Check if any filesystem is running degraded
sudo btrfs filesystem show          # Look for "missing" devices
sudo btrfs device usage /mountpoint # "Device missing" should be 0.00B

# Check mount options for "degraded" flag
mount | grep btrfs | grep degraded  # Should return nothing normally
```

---

## 4. Immediate Response Checklist

When you suspect a drive failure:

- [ ] **1. Identify the failed array and device**
  ```bash
  sudo btrfs filesystem show        # Shows "missing" for failed device
  sudo btrfs device stats /mountpoint  # Non-zero errors point to failing drive
  sudo dmesg | tail -50             # Recent kernel messages about I/O errors
  ```

- [ ] **2. Confirm system is running degraded (not crashed)**
  ```bash
  mount | grep btrfs                # All expected mounts present?
  df -h /mountpoint                 # Filesystems responding?
  ```

- [ ] **3. Verify surviving drive health**
  ```bash
  sudo smartctl -a /dev/<surviving-drive>
  ```

- [ ] **4. Do NOT reboot** unless absolutely necessary -- degraded BTRFS may fail to mount without `rootflags=degraded` or `mount -o degraded`

- [ ] **5. Back up critical data** if the surviving drive shows any SMART warnings

- [ ] **6. Procure replacement drive** -- must be equal or larger capacity than the failed device

---

## 5. Recovery Procedures

### 5a. Boot Drive Failure (NVMe/SSD with ESP)

If your primary boot drive fails but you have a mirrored ESP:

1. **Boot from backup ESP** -- select the fallback boot entry in your UEFI/BIOS boot menu
2. **Use Safe Mode / degraded entry** -- boot with `rootflags=subvol=/@,degraded` if needed
3. **Verify degraded operation**:
   ```bash
   mount | grep btrfs
   sudo btrfs filesystem show /
   ```

4. **Install replacement drive** and clone partition table from surviving drive:
   ```bash
   sudo sfdisk -d /dev/<surviving-drive> | sudo sfdisk /dev/<new-drive>
   ```

5. **Create partitions on new drive**:
   ```bash
   # Swap (if applicable)
   sudo mkswap /dev/<new-drive-swap-partition>

   # ESP
   sudo mkfs.vfat -F32 /dev/<new-drive-esp-partition>

   # Sync ESP contents
   sudo mkdir -p /mnt/new-esp
   sudo mount /dev/<new-drive-esp-partition> /mnt/new-esp
   sudo rsync -aHAXS /boot/ /mnt/new-esp/
   sudo umount /mnt/new-esp
   ```

6. **Replace the failed BTRFS device**:
   ```bash
   sudo btrfs replace start <devid> /dev/<new-drive-btrfs-partition> / -B
   ```

7. **Re-register UEFI boot entry**:
   ```bash
   sudo efibootmgr --create --disk /dev/<new-drive> --part <esp-partition-number> \
     --loader '\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI' \
     --label "<your-boot-label>" --unicode
   ```

8. **Update fstab** with new partition UUIDs:
   ```bash
   blkid /dev/<new-drive-esp-partition>    # Get new ESP UUID
   blkid /dev/<new-drive-swap-partition>   # Get new swap UUID
   sudo vim /etc/fstab                     # Update UUIDs
   ```

9. **Verify ESP sync**:
   ```bash
   sudo /usr/local/bin/esp-sync.sh   # Or your ESP sync script
   ```

### 5b. Data Drive Failure (No ESP)

For SATA SSDs, HDDs, or data-only NVMe:

1. **Verify the array is degraded** (system may keep running from mirror):
   ```bash
   sudo btrfs filesystem show /mountpoint
   ```

2. **If the mount failed on reboot**, mount manually with degraded:
   ```bash
   sudo mount -o degraded UUID=<your-uuid> /mountpoint
   ```

3. **Install replacement drive**:
   ```bash
   sudo wipefs -a /dev/<new-drive>
   ```

4. **Replace the failed device**:
   ```bash
   sudo btrfs replace start <devid> /dev/<new-drive> /mountpoint -B
   ```

5. **Verify**:
   ```bash
   sudo btrfs filesystem show /mountpoint   # Both devices present
   sudo btrfs device stats /mountpoint      # All zeros
   sudo btrfs scrub start -B /mountpoint    # Full integrity check
   ```

**Time estimates**: Replacement time depends on data volume and drive speed. NVMe arrays with ~375 GiB take ~15-30 minutes. Large HDD arrays (10+ TiB) can take 24-72 hours.

### 5c. Incomplete RAID Conversion (Mixed RAID-0/RAID-1)

If a drive fails while a RAID-0 to RAID-1 balance is incomplete:

- **RAID-1 chunks**: Recoverable from surviving drive
- **RAID-0 chunks**: **Permanently lost** -- no recovery possible

**Partial recovery attempt** (salvage RAID-1 data only):
```bash
# Mount degraded read-only (may fail if metadata was on RAID-0 chunks)
sudo mount -o degraded,ro UUID=<your-uuid> /mnt/recovery

# Copy recoverable data
rsync -avP /mnt/recovery/ /path/to/safe/location/

# If mount fails:
sudo btrfs rescue super-recover /dev/<surviving-drive>
```

---

## 6. Post-Replacement Verification

Run these checks after ANY drive replacement:

### 6a. BTRFS Integrity

```bash
# Full scrub (reads every block, verifies checksums)
sudo btrfs scrub start -B /mountpoint
sudo btrfs scrub status /mountpoint
```

### 6b. Device Stats

```bash
sudo btrfs device stats /mountpoint
# Every counter must be 0
```

### 6c. Filesystem Health

```bash
sudo btrfs filesystem show    # Both devices present, balanced usage
sudo btrfs filesystem df /mountpoint  # Correct RAID profile
```

### 6d. ESP Sync (boot drive replacement only)

```bash
# Verify both ESPs have identical content
diff <(sudo find /boot -type f -exec md5sum {} \;) \
     <(sudo find <backup-esp-mount> -type f -exec md5sum {} \;)

# If different, resync
sudo /usr/local/bin/esp-sync.sh
```

### 6e. Reboot and Verify

```bash
sudo reboot

# After reboot:
mount | grep btrfs           # All mounts present
sudo btrfs filesystem show   # All devices present, no "missing"
efibootmgr                   # Boot order correct (if applicable)
```

---

## 7. Quick Reference Templates

### UUID Reference Template

| Purpose | UUID | Device(s) |
|---------|------|-----------|
| `<array-name>` BTRFS | `<uuid>` | `<dev1>`, `<dev2>` |
| ESP primary | `<uuid>` | `<device>` |
| ESP backup | `<uuid>` | `<device>` |
| Swap | `<uuid>` | `<device>` |

### Serial Number Reference Template

| Device | Serial | Model |
|--------|--------|-------|
| `<device>` | `<serial>` | `<model>` |

### Essential Commands Cheat Sheet

```bash
# --- HEALTH CHECK ---
sudo btrfs device stats /mountpoint    # Error counters
sudo btrfs filesystem show             # All arrays, device status
sudo smartctl -a /dev/<device>         # SMART health

# --- DEGRADED OPERATIONS ---
sudo btrfs filesystem show             # Find "missing" device
sudo mount -o degraded UUID=<uuid> /mp # Mount with one drive missing

# --- REPLACEMENT ---
sudo wipefs -a /dev/<new-drive>                          # Clean new drive
sudo btrfs replace start <devid> /dev/<new> /mp -B       # Start replacement
sudo btrfs replace status /mountpoint                     # Check progress

# --- PARTITION CLONING (boot drives) ---
sudo sfdisk -d /dev/<source> | sudo sfdisk /dev/<dest>   # Clone layout
sudo mkswap /dev/<swap-partition>                          # Create swap
sudo mkfs.vfat -F32 /dev/<esp-partition>                   # Create ESP

# --- ESP MANAGEMENT ---
efibootmgr -v                          # View boot entries
sudo efibootmgr --create --disk /dev/<drive> --part <N> \
  --loader '\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI' \
  --label "<label>" --unicode

# --- VERIFICATION ---
sudo btrfs scrub start -B /mountpoint  # Full integrity check
```

### Replacement Drive Specification Template

| Array | Required Spec | Minimum Size | Interface |
|-------|--------------|--------------|-----------|
| `<array>` | `<spec>` | `<minimum-size>` | `<interface>` |

---

## Reference Example

See [examples/author-storage-reference.md](examples/author-storage-reference.md) for a fully documented CachyOS system with NVMe RAID-1, SSD RAID-1, HDD RAID-1, complete UUIDs, PARTUUIDs, serial numbers, and detailed recovery procedures for each array.
