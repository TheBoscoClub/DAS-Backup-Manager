> **Note**: This is the author's specific CachyOS system. See [STORAGE-ARCHITECTURE-AND-RECOVERY.md](../STORAGE-ARCHITECTURE-AND-RECOVERY.md) for the generic guide.

# Storage Architecture & Emergency Recovery Guide

> **System**: CachyOS (Arch-based) on ASUS ROG Crosshair VIII Dark Hero
> **Boot**: systemd-boot (NOT GRUB)
> **Filesystem**: BTRFS on all arrays, RAID-1 mirrors
> **Last verified**: 2026-02-01
> **HDD RAID-0->RAID-1 balance**: IN PROGRESS (see Section 7 WARNING)

---

## Table of Contents

1. [Architecture Overview](#1-architecture-overview)
2. [RAID Array Reference](#2-raid-array-reference)
3. [Failure Detection](#3-failure-detection)
4. [Immediate Response Checklist](#4-immediate-response-checklist)
5. [Recovery: NVMe Failure](#5-recovery-nvme-failure)
6. [Recovery: SSD Failure](#6-recovery-ssd-failure)
7. [Recovery: HDD Failure](#7-recovery-hdd-failure)
8. [Post-Replacement Verification](#8-post-replacement-verification)
9. [Quick Reference Card](#9-quick-reference-card)
10. [Offline Backup Plan](#10-offline-backup-plan)

---

## 1. Architecture Overview

### Device Inventory

| Device | Model | Serial | Size | Role | BTRFS devid |
|--------|-------|--------|------|------|-------------|
| nvme0n1 | WD Black SN850X (WDS100T1X0E-00AFY0) | 204445805771 | 1 TB (931.5G) | NVMe RAID-1 primary, boot drive | 1 |
| nvme1n1 | WD Black SN850X (WDS100T1X0E-00AFY0) | 20465F802394 | 1 TB (931.5G) | NVMe RAID-1 secondary, backup ESP | 2 |
| sdb | Samsung SSD 860 PRO 1TB | S5HVNA0N303556E | 1 TB (953.9G) | SSD RAID-1 (devid 1) | 1 |
| sdc | Samsung SSD 850 EVO mSATA 1TB | S246NWAG500270V | 1 TB (931.5G) | SSD RAID-1 (devid 2) | 2 |
| sda | Seagate Exos X24 (ST24000DM001-3Y7103) | ZXA0MHSK | 24 TB (21.83 TiB) | HDD RAID-1 (devid 1) | 1 |
| sdd | Seagate Exos X24 (ST24000DM001-3Y7103) | ZXA0V0EY | 24 TB (21.83 TiB) | HDD RAID-1 (devid 2) | 2 |

### BTRFS Filesystem UUIDs

| Array | UUID | Label |
|-------|------|-------|
| NVMe RAID-1 | `20b5fa7e-d8c0-4035-ae45-f80263073a96` | (none) |
| SSD RAID-1 | `2638d087-0be1-436e-bfe4-8d6551ec02be` | `sata_raid0` |
| HDD RAID-1 | `8b66e847-4273-4e2a-ad53-b312b3b3ee6d` | (none) |

### Array -> Subvolume -> Mount Point Diagram

```
+---------------------------------------------------------------------+
|                        NVMe RAID-1 (BTRFS)                          |
|               nvme0n1p2 + nvme1n1p2 (926G each)                     |
|            UUID: 20b5fa7e-d8c0-4035-ae45-f80263073a96               |
|                                                                     |
|  +---------+ +-------+ +------+ +-------+ +--------------+         |
|  | @  -> / | |@home  | |@root | | @log  | |@audiobooks-db|         |
|  |         | |-> /   | |-> /  | |-> /   | |-> /var/lib/  |         |
|  |         | | home  | | root | | var/  | |audiobooks/db |         |
|  |         | |       | |      | | log   | |              |         |
|  +---------+ +-------+ +------+ +-------+ +--------------+         |
|                                                                     |
|  Also: @tmp, @var-tmp (disabled -- now tmpfs)                       |
|  Snapper: root, home, root-home, var-log                            |
+---------------------------------------------------------------------+

+---------------------------------------------------------------------+
|                     ESP Dual-Boot Architecture                      |
|                                                                     |
|  nvme0n1p3 (1.5G)              nvme1n1p3 (1.5G)                    |
|  UUID: 129B-4CA4               UUID: 7DE5-027D                     |
|  Mount: /boot (primary)        Mount: /mnt/esp-backup              |
|                                                                     |
|  UEFI Boot0006 <- nvme0n1p3   UEFI Boot0000 <- nvme1n1p3          |
|  (BootCurrent)                 (automatic fallback)                 |
|                                                                     |
|  /boot/loader/entries:         Synced via /usr/local/bin/esp-sync  |
|   +- linux-cachyos.conf       Triggered by pacman hook:            |
|   +- linux-cachyos-fallback   /etc/pacman.d/hooks/esp-mirror.hook  |
|   +- linux-cachyos-safe                                             |
|   +- linux-cachyos-cli        rsync --delete /boot/ /mnt/esp-backup|
+---------------------------------------------------------------------+

+---------------------------------------------------------------------+
|                        SSD RAID-1 (BTRFS)                           |
|                     sdb + sdc (whole-disk)                          |
|            UUID: 2638d087-0be1-436e-bfe4-8d6551ec02be               |
|                                                                     |
|  +-----------+ +----------+ +---------+ +-------+ +---------------+ |
|  | @opt      | | @srv     | | @cache  | | @hibp | |VirtualMachines| |
|  | -> /opt   | | -> /srv  | |-> /var/ | |-> ~/. | |-> /hddRaid1/  | |
|  |           | |          | |  cache  | |local/ | |VirtualMachines| |
|  |           | |          | |         | |share/ | |               | |
|  |           | |          | |         | |hibp-  | |               | |
|  |           | |          | |         | |checker| |               | |
|  +-----------+ +----------+ +---------+ +-------+ +---------------+ |
|  Snapper: opt, srv, var-cache, hibp-data                            |
+---------------------------------------------------------------------+

+---------------------------------------------------------------------+
|                     HDD RAID (BTRFS) -- 24TB x 2                    |
|                     sda + sdd (whole-disk)                          |
|            UUID: 8b66e847-4273-4e2a-ad53-b312b3b3ee6d               |
|                                                                     |
|  WARNING: RAID-0->RAID-1 BALANCE IN PROGRESS (see Section 7)       |
|  Data currently: RAID-0 (6.78 TiB) + RAID-1 (3.97 TiB)            |
|                                                                     |
|  Top-level subvolumes:                                              |
|  +- ClaudeCodeProjects -> /hddRaid1/ClaudeCodeProjects             |
|  |  +- Audiobook-Manager    +- hibp-project                        |
|  |  +- Asus-DarkHero        +- local-ai-hub                        |
|  |  +- CachyOS-Kernel       +- mcp-workspace                       |
|  |  +- claude-code-streaming +- skt-smt                             |
|  |  +- FreeBSD              +- steam-sam-optimizer                   |
|  |  +- General-Chat         +- test-skill                           |
|  |  +- libvirt-vm-manager   +- zsh-stuff                            |
|  |  +- scx-autoswitch       +- .repo-templates                     |
|  +- Audiobooks -> /hddRaid1/Audiobooks                              |
|  +- SteamLibrary -> /hddRaid1/SteamLibrary                         |
|  +- SteamLibrary-local -> ~/.local/share/Steam                      |
|  +- ISOs -> /hddRaid1/ISOs                                         |
|  +- ai-models-{text,image,video,audio,multimodal}                   |
|  +- VirtualMachines (migrated to SSD RAID 2026-01-12)              |
+---------------------------------------------------------------------+

+---------------------------------------------------------------------+
|                        Swap Configuration                           |
|                                                                     |
|  nvme0n1p1: 4G swap  UUID: ddba4cee-f2b9-4820-96bf-46ac82c6e779   |
|  nvme1n1p1: 4G swap  UUID: 1966b9f0-0828-4d99-9cb8-e5138032f67b   |
|  zram0:     125.7G   UUID: 08152df1-c7eb-4485-8eeb-19d4b1bade94   |
|                                                                     |
|  tmpfs: /tmp (32G), /var/tmp (16G) -- NVMe wear reduction          |
+---------------------------------------------------------------------+
```

---

## 2. RAID Array Reference

### 2a. NVMe RAID-1 -- Boot & Root (Most Critical)

**Devices**: nvme0n1p2 (926G, devid 1) + nvme1n1p2 (926G, devid 2)
**BTRFS UUID**: `20b5fa7e-d8c0-4035-ae45-f80263073a96`
**Profile**: Data RAID-1, Metadata RAID-1
**Converted from RAID-0**: 2026-01-31
**Current usage**: 372.78 GiB used / 547.20 GiB free

#### Partition Layout (identical on both drives)

```
Partition   Start Sector   Size    Type                    Purpose
--------------------------------------------------------------------
p3          2048           1.5G    C12A7328 (EFI System)   ESP / systemd-boot
p1          3145728        4G      0657FD6D (Linux Swap)   Swap partition
p2          11534336       926G    0FC63DAF (Linux FS)     BTRFS RAID-1 root
```

#### Partition UUIDs (GPT PARTUUIDs)

| Partition | nvme0n1 PARTUUID | nvme1n1 PARTUUID |
|-----------|------------------|------------------|
| p1 (swap) | `DA9EB6F7-6C4F-4D54-880D-337FE5A45171` | `94F34602-51A2-444C-B930-A265ADA6BFDF` |
| p2 (BTRFS)| `ADFDF354-30D4-47F8-A98C-C0BB689E0EF8` | `37E61C6B-9448-46E9-917D-77D73DA28A4B` |
| p3 (ESP)  | `CA1C0553-72EB-4117-BAC4-981927B721A6` | `CC7834C1-A4C8-4090-B396-2EAB7E9CF463` |

#### UEFI Boot Entries

| Entry | Name | Target Drive | GPT PARTUUID | EFI Path |
|-------|------|-------------|-------------|----------|
| Boot0006 | Linux Boot Manager | nvme0n1p3 | `CA1C0553-...` | `\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI` |
| Boot0000 | Linux Boot Manager (NVMe1) | nvme1n1p3 | `CC7834C1-...` | `\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI` |
| Boot0007 | UEFI OS | nvme0n1p3 | `CA1C0553-...` | `\EFI\BOOT\BOOTX64.EFI` |

**Boot order**: `0006,0000,0007,0001,0002,0003`
**Current boot**: Boot0006 (nvme0n1p3)

#### ESP Sync Chain

1. Pacman installs/upgrades kernel, initramfs, or bootloader
2. Pacman hook `/etc/pacman.d/hooks/esp-mirror.hook` fires (PostTransaction)
3. Calls `/usr/local/bin/esp-sync.sh`
4. `rsync -aHAXS --delete /boot/ /mnt/esp-backup/`
5. Both ESPs are now identical

#### Boot Entries (systemd-boot)

**Default** (`linux-cachyos.conf`):
```
title Linux Cachyos
options root=UUID=20b5fa7e-d8c0-4035-ae45-f80263073a96 rw rootflags=subvol=/@ zswap.enabled=0 nowatchdog quiet splash
linux /vmlinuz-linux-cachyos
initrd /initramfs-linux-cachyos.img
```

**Safe Mode** (`linux-cachyos-safe.conf`) -- for degraded boot:
```
title   CachyOS (Safe Mode)
options root=UUID=20b5fa7e-d8c0-4035-ae45-f80263073a96 rw rootflags=subvol=/@,degraded btrfs.device_scan_wait=1 nomodeset
linux   /vmlinuz-linux-cachyos
initrd  /amd-ucode.img
initrd  /initramfs-linux-cachyos.img
```

**CLI Only** (`linux-cachyos-cli.conf`) -- no GUI:
```
title CachyOS (CLI Only)
options root=UUID=20b5fa7e-d8c0-4035-ae45-f80263073a96 rw rootflags=subvol=/@ zswap.enabled=0 nowatchdog systemd.unit=multi-user.target
linux /vmlinuz-linux-cachyos
initrd /amd-ucode.img
initrd /initramfs-linux-cachyos.img
```

### 2b. SSD RAID-1 -- Services & VMs

**Devices**: sdb (Samsung 860 PRO, 953.87G, devid 1) + sdc (Samsung 850 EVO mSATA, 931.51G, devid 2)
**BTRFS UUID**: `2638d087-0be1-436e-bfe4-8d6551ec02be`
**Label**: `sata_raid0` (legacy name -- actually RAID-1)
**Profile**: Data RAID-1, Metadata RAID-1
**Current usage**: 150.47 GiB used / 780 GiB free

**Important**: Drives are different sizes (953.87G vs 931.51G). Usable capacity is limited by the smaller drive.

#### Subvolumes

| Subvolume | Mount Point | Options | Purpose |
|-----------|-------------|---------|---------|
| @opt | /opt | ssd,compress=zstd:3 | Installed software |
| @srv | /srv | ssd,compress=zstd:1 | Server data |
| @cache | /var/cache | ssd,nodatacow | Package cache |
| @hibp | ~/.local/share/hibp-checker | ssd,compress=zstd:1 | HIBP password data |
| VirtualMachines | /hddRaid1/VirtualMachines | ssd,nodatacow,commit=30 | libvirt QCOW2 images |

### 2c. HDD RAID -- Mass Storage

**Devices**: sda (Exos X24, 21.83 TiB, devid 1) + sdd (Exos X24, 21.83 TiB, devid 2)
**BTRFS UUID**: `8b66e847-4273-4e2a-ad53-b312b3b3ee6d`
**Profile**: Mixed -- RAID-0 (legacy) + RAID-1 (new)
**Current usage**: 9.03 TiB total used / ~15.3 TiB free

> **CONVERSION HISTORY**: This array was originally RAID-0. A `btrfs balance` converting Data to RAID-1 was started. As of 2026-02-01, the balance is **still in progress**:
> - Data,RAID0: 6.78 TiB (4.99 TiB used) -- **not yet converted**
> - Data,RAID1: 3.97 TiB (3.96 TiB used) -- **already converted**
> - Metadata,RAID1: fully converted
> - System,RAID1: fully converted

#### Top-Level Subvolumes (non-snapshot)

| Subvolume | Mount Point | compress | Notes |
|-----------|-------------|----------|-------|
| ClaudeCodeProjects | /hddRaid1/ClaudeCodeProjects | zstd:3 | Parent for all Claude projects |
| Audiobooks | /hddRaid1/Audiobooks | no | Audiobook files |
| SteamLibrary | /hddRaid1/SteamLibrary | no | Steam games (secondary) |
| SteamLibrary-local | ~/.local/share/Steam | no | Steam games (primary) |
| ISOs | /hddRaid1/ISOs | no | ISO images |
| ai-models-text | (not mounted) | zstd:1 | AI model storage |
| ai-models-image | (not mounted) | zstd:1 | AI model storage |
| ai-models-video | (not mounted) | zstd:1 | AI model storage |
| ai-models-audio | (not mounted) | zstd:1 | AI model storage |
| ai-models-multimodal | (not mounted) | zstd:1 | AI model storage |

#### Project Subvolumes (under ClaudeCodeProjects)

Each is an independent BTRFS subvolume with its own Snapper config:

Audiobook-Manager, hibp-project, Asus-DarkHero, CachyOS-Kernel, claude-code-streaming-feature, FreeBSD, General-Chat, libvirt-vm-manager, local-ai-hub, mcp-workspace, scx-autoswitch, skt-smt, steam-sam-optimizer, test-skill, zsh-stuff, .repo-templates

---

## 3. Failure Detection

### 3a. SMART Monitoring

Check NVMe drives:
```bash
sudo smartctl -a /dev/nvme0n1    # Primary NVMe
sudo smartctl -a /dev/nvme1n1    # Secondary NVMe
```

Check SATA drives:
```bash
sudo smartctl -a /dev/sdb    # Samsung 860 PRO
sudo smartctl -a /dev/sdc    # Samsung 850 EVO
sudo smartctl -a /dev/sda    # Seagate Exos X24 (sn: ZXA0MHSK)
sudo smartctl -a /dev/sdd    # Seagate Exos X24 (sn: ZXA0V0EY)
```

**Key SMART attributes to watch**:
- NVMe: `Percentage Used`, `Media and Data Integrity Errors`, `Error Information Log Entries`
- SATA SSD: `Reallocated_Sector_Ct`, `Wear_Leveling_Count`, `Runtime_Bad_Block`
- SATA HDD: `Reallocated_Sector_Ct`, `Current_Pending_Sector`, `Offline_Uncorrectable`, `UDMA_CRC_Error_Count`

### 3b. BTRFS Device Stats

```bash
# Check all arrays -- ANY non-zero value means a problem
sudo btrfs device stats /           # NVMe RAID-1
sudo btrfs device stats /opt        # SSD RAID-1
sudo btrfs device stats /hddRaid1   # HDD RAID

# Expected output (healthy):
# [/dev/nvme0n1p2].write_io_errs    0
# [/dev/nvme0n1p2].read_io_errs     0
# [/dev/nvme0n1p2].flush_io_errs    0
# [/dev/nvme0n1p2].corruption_errs  0
# [/dev/nvme0n1p2].generation_errs  0
```

**Interpretation**:
- `write_io_errs > 0`: Drive can't write -- likely failing hardware
- `read_io_errs > 0`: Drive can't read -- data may be corrupt, BTRFS will use mirror
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

# Common failure patterns:
# "blk_update_request: I/O error, dev sda, sector NNNN"     <- HDD sector failure
# "ata3: COMRESET failed"                                     <- SATA link failure
# "BTRFS error (device nvme0n1p2): bdev /dev/nvme0n1p2 errs" <- BTRFS detected device error
# "nvme nvme0: I/O Cmd(0x02) error"                          <- NVMe read failure
```

### 3d. Degraded Mount Detection

```bash
# Check if any filesystem is running degraded
sudo btrfs filesystem show          # Look for "missing" devices
sudo btrfs device usage /           # "Device missing" should be 0.00B
sudo btrfs device usage /opt
sudo btrfs device usage /hddRaid1

# Check mount options for "degraded" flag
mount | grep btrfs | grep degraded  # Should return nothing normally
```

---

## 4. Immediate Response Checklist

When you suspect a drive failure:

- [ ] **1. Identify the failed array and device**
  ```bash
  sudo btrfs filesystem show        # Shows "missing" for failed device
  sudo btrfs device stats /         # Non-zero errors point to failing drive
  sudo btrfs device stats /opt
  sudo btrfs device stats /hddRaid1
  sudo dmesg | tail -50             # Recent kernel messages about I/O errors
  ```

- [ ] **2. Confirm system is running degraded (not crashed)**
  ```bash
  mount | grep btrfs                # All expected mounts present?
  df -h / /opt /hddRaid1            # Filesystems responding?
  ```

- [ ] **3. Verify surviving drive health**
  ```bash
  # Whichever drive is still alive -- run SMART on it
  sudo smartctl -a /dev/<surviving-drive>
  ```

- [ ] **4. Do NOT reboot** unless absolutely necessary (degraded BTRFS may fail to mount without `rootflags=degraded`)

- [ ] **5. Back up critical data** if the surviving drive shows any SMART warnings

- [ ] **6. Procure replacement drive**

  | Failed Drive | Replacement Spec | Minimum Size |
  |-------------|------------------|--------------|
  | nvme0n1 or nvme1n1 | WD Black SN850X 1TB NVMe M.2 2280 (WDS100T1X0E) | 931.5G (1 TB) |
  | sdb (860 PRO) | Any 1TB SATA SSD | 931.51G (1 TB) |
  | sdc (850 EVO) | Any 1TB SATA SSD | 931.51G (1 TB) |
  | sda or sdd | Seagate Exos X24 24TB (ST24000DM001) | 21.83 TiB (24 TB) |

---

## 5. Recovery: NVMe Failure

### 5a. nvme0n1 Fails (Primary Boot Drive)

**Impact**: System loses primary ESP (/boot) and one leg of root RAID-1.
**Auto-recovery**: UEFI falls through Boot0006 -> Boot0000 (nvme1n1p3), which has identical ESP contents.

#### Step 1: Boot from backup NVMe

The UEFI boot order already includes Boot0000 pointing to nvme1n1p3. If BIOS doesn't auto-fallback:
1. Enter BIOS (DEL at POST)
2. Select "Linux Boot Manager (NVMe1)" manually
3. At systemd-boot menu, select **"CachyOS (Safe Mode)"** which has `rootflags=subvol=/@,degraded`

If Safe Mode entry is missing, press `e` on any entry and append to the options line:
```
rootflags=subvol=/@,degraded
```

#### Step 2: Verify degraded operation

```bash
# Confirm system booted and root is mounted
mount | grep btrfs
sudo btrfs filesystem show /
# Should show: "*** Some devices missing"

# Verify data integrity
sudo btrfs device stats /
```

#### Step 3: Install replacement NVMe

1. Power off, install new NVMe in slot 0 (where nvme0n1 was)
2. Boot from nvme1n1 (backup -- may need BIOS selection)

#### Step 4: Clone partition table

```bash
# Dump partition layout from surviving drive and apply to new drive
sudo sfdisk -d /dev/nvme1n1 | sudo sfdisk /dev/nvme0n1

# Verify
sudo sfdisk -l /dev/nvme0n1
```

#### Step 5: Create swap partition

```bash
sudo mkswap -L swap2 /dev/nvme0n1p1
# Note the new UUID -- update fstab if you want both swaps active
```

#### Step 6: Create ESP

```bash
sudo mkfs.vfat -F32 -n EFI /dev/nvme0n1p3
```

#### Step 7: Mount new ESP and sync contents

```bash
sudo mkdir -p /mnt/new-esp
sudo mount /dev/nvme0n1p3 /mnt/new-esp
sudo rsync -aHAXS /boot/ /mnt/new-esp/
sudo umount /mnt/new-esp
```

#### Step 8: Replace the failed BTRFS device

```bash
# Find the devid of the missing device
sudo btrfs filesystem show /
# Look for the line with "*** Some devices missing" -- note the devid

# Start replacement (devid 1 was nvme0n1p2)
sudo btrfs replace start 1 /dev/nvme0n1p2 / -B
# -B runs in foreground (recommended for monitoring)
# This will take ~15-30 minutes for ~375 GiB of data

# Monitor progress if running without -B:
sudo btrfs replace status /
```

#### Step 9: Re-register UEFI boot entry

```bash
# Get the new ESP partition PARTUUID
PARTUUID=$(blkid -s PARTUUID -o value /dev/nvme0n1p3)

# Register new boot entry
sudo efibootmgr --create --disk /dev/nvme0n1 --part 3 \
  --loader '\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI' \
  --label "Linux Boot Manager" --unicode

# Set boot order (new entry first, then nvme1n1 as fallback)
# Check efibootmgr output for the new entry number, then:
sudo efibootmgr -o XXXX,0000,0007,0001,0002,0003
# Replace XXXX with the new entry number
```

#### Step 10: Update fstab

```bash
# Get new ESP UUID
NEW_ESP_UUID=$(blkid -s UUID -o value /dev/nvme0n1p3)

# Update /boot mount to use new UUID (if changed)
sudo vim /etc/fstab
# UUID=<NEW_ESP_UUID>  /boot  vfat  defaults,umask=0077  0 2

# Update esp-backup to point to nvme1n1p3 (should already be correct)
```

#### Step 11: Verify ESP sync

```bash
# Run sync manually
sudo /usr/local/bin/esp-sync.sh

# Verify both ESPs are identical
diff <(sudo ls -laR /boot/) <(sudo ls -laR /mnt/esp-backup/)
```

### 5b. nvme1n1 Fails (Backup Boot Drive)

**Impact**: System boots normally from nvme0n1 (primary). Lost: backup ESP + one RAID-1 leg.
**Urgency**: Medium -- system is fully functional but unprotected.

#### Steps

1. Boot normally (nvme0n1 is primary boot)
2. Verify degraded: `sudo btrfs filesystem show /`
3. Install replacement NVMe in slot 1
4. Clone partition table:
   ```bash
   sudo sfdisk -d /dev/nvme0n1 | sudo sfdisk /dev/nvme1n1
   ```
5. Create swap: `sudo mkswap /dev/nvme1n1p1`
6. Create ESP: `sudo mkfs.vfat -F32 /dev/nvme1n1p3`
7. Replace BTRFS device:
   ```bash
   sudo btrfs replace start 2 /dev/nvme1n1p2 / -B
   ```
8. Mount backup ESP and sync:
   ```bash
   sudo mount /dev/nvme1n1p3 /mnt/esp-backup
   sudo /usr/local/bin/esp-sync.sh
   ```
9. Update fstab UUID for /mnt/esp-backup if needed:
   ```bash
   NEW_UUID=$(blkid -s UUID -o value /dev/nvme1n1p3)
   # Update: UUID=<NEW_UUID>  /mnt/esp-backup  vfat  defaults,umask=0077,nofail  0 2
   ```
10. Re-register UEFI Boot0000 fallback entry:
    ```bash
    sudo efibootmgr --create --disk /dev/nvme1n1 --part 3 \
      --loader '\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI' \
      --label "Linux Boot Manager (NVMe1)" --unicode
    ```

---

## 6. Recovery: SSD Failure

**Devices**: sdb (Samsung 860 PRO, devid 1) + sdc (Samsung 850 EVO mSATA, devid 2)
**Impact**: /opt, /srv, /var/cache, HIBP data, VirtualMachines mount from surviving drive.
**System continues running** -- no reboot needed.

> **Size note**: sdb is 953.87G, sdc is 931.51G. Replacement must be >= 931.51G (1 TB class).

### Either Drive Fails

#### Step 1: Identify which drive failed

```bash
sudo btrfs filesystem show /opt
# Shows which devid is missing

sudo btrfs device stats /opt
# Shows error counters
```

#### Step 2: Install replacement

Power off if needed, install new SATA SSD.

#### Step 3: Prepare new drive

```bash
# Clean any existing signatures
sudo wipefs -a /dev/sdX    # Replace sdX with new drive letter

# BTRFS replace uses the whole disk -- no partitioning needed
```

#### Step 4: Replace the failed device

```bash
# Identify the missing devid
sudo btrfs filesystem show /opt

# Replace (example: devid 2 was sdc)
sudo btrfs replace start 2 /dev/sdX /opt -B
# ~150 GiB of data -- should complete in ~10-15 minutes on SATA
```

#### Step 5: Verify

```bash
sudo btrfs filesystem show /opt     # Both devices present
sudo btrfs device stats /opt        # All zeros
sudo btrfs scrub start -B /opt      # Full integrity check
```

---

## 7. Recovery: HDD Failure

**Devices**: sda (Exos X24, devid 1) + sdd (Exos X24, devid 2)
**Mount**: /hddRaid1 and all subvolumes

> ### WARNING: RAID-0 DATA LOSS RISK
>
> **As of 2026-02-01, the RAID-0 -> RAID-1 balance conversion is INCOMPLETE.**
>
> ```
> Data,RAID0: total=6.78TiB, used=4.99TiB   <- NOT YET MIRRORED
> Data,RAID1: total=3.97TiB, used=3.96TiB   <- Already mirrored
> ```
>
> **If either HDD fails while RAID-0 chunks remain, ALL data in RAID-0 chunks is PERMANENTLY LOST.**
> **There is NO recovery for RAID-0 data when one drive dies.**
>
> #### Check current status:
> ```bash
> sudo btrfs filesystem df /hddRaid1
> # If you see ANY line with "Data, RAID0" -- data loss risk exists
> # Once ALL data shows "Data, RAID1" -- you are safe
>
> # Check if balance is running:
> sudo btrfs balance status /hddRaid1
> ```

### If Balance Is Complete (All RAID-1)

Standard recovery -- same as SSD procedure but much slower:

```bash
# 1. Identify failed drive
sudo btrfs filesystem show /hddRaid1

# 2. If system won't mount /hddRaid1 after reboot, add degraded:
#    Edit fstab: add "degraded" to HDD mount options
#    Or mount manually:
sudo mount -o degraded,noatime,nossd,space_cache=v2 \
  UUID=8b66e847-4273-4e2a-ad53-b312b3b3ee6d /hddRaid1

# 3. Install replacement 24TB drive
sudo wipefs -a /dev/sdX

# 4. Replace (this will take DAYS for 24TB drives)
sudo btrfs replace start <devid> /dev/sdX /hddRaid1
# Monitor: sudo btrfs replace status /hddRaid1

# Expect 24-72 hours depending on data volume (~9 TiB to sync)
```

### If Balance Is Incomplete (Mixed RAID-0/RAID-1)

If a drive fails while RAID-0 chunks exist:

1. **RAID-1 chunks**: Recoverable from surviving drive
2. **RAID-0 chunks**: **PERMANENTLY LOST** -- no recovery possible

**Partial recovery attempt** (salvage RAID-1 data only):
```bash
# Mount degraded (may fail if critical metadata was on RAID-0 chunks)
sudo mount -o degraded,ro UUID=8b66e847-4273-4e2a-ad53-b312b3b3ee6d /mnt/recovery

# Copy what you can to another drive
rsync -avP /mnt/recovery/ /path/to/backup/

# If mount fails entirely, try btrfs-rescue:
sudo btrfs rescue super-recover /dev/sda   # or sdd (surviving drive)
```

**Replacement must be**: >= 21.83 TiB (24 TB class Seagate Exos or equivalent)

---

## 8. Post-Replacement Verification

Run these checks after ANY drive replacement:

### 8a. BTRFS Integrity

```bash
# Full scrub (reads every block on both drives, verifies checksums)
sudo btrfs scrub start -B /          # NVMe -- ~15-30 min
sudo btrfs scrub start -B /opt       # SSD -- ~5-10 min
sudo btrfs scrub start -B /hddRaid1  # HDD -- hours/days

# Check results
sudo btrfs scrub status /
sudo btrfs scrub status /opt
sudo btrfs scrub status /hddRaid1
```

### 8b. Device Stats (All Zeros)

```bash
sudo btrfs device stats /
sudo btrfs device stats /opt
sudo btrfs device stats /hddRaid1
# Every counter must be 0
```

### 8c. Filesystem Health

```bash
sudo btrfs filesystem show
# Both devices present, balanced usage

sudo btrfs filesystem df /
sudo btrfs filesystem df /opt
sudo btrfs filesystem df /hddRaid1
# Correct RAID profiles (RAID1 for all)
```

### 8d. ESP Sync (NVMe replacement only)

```bash
# Verify both ESPs have identical content
diff <(sudo find /boot -type f -exec md5sum {} \;) \
     <(sudo find /mnt/esp-backup -type f -exec md5sum {} \;)

# If different, resync:
sudo /usr/local/bin/esp-sync.sh
```

### 8e. Snapper Configuration

```bash
# Verify all snapper configs are intact
sudo snapper list-configs

# Expected configs (25 total):
# root, home, root-home, var-log (NVMe)
# opt, srv, var-cache, hibp-data (SSD)
# claude-code, Audiobooks, steam-library, isos, + all project subvolumes (HDD)
```

### 8f. Reboot and Verify

```bash
# Reboot to confirm clean boot
sudo reboot

# After reboot, verify:
mount | grep btrfs           # All mounts present
sudo btrfs filesystem show   # All devices present, no "missing"
efibootmgr                   # Boot order correct
```

---

## 9. Quick Reference Card

### All UUIDs at a Glance

| Purpose | UUID | Device(s) |
|---------|------|-----------|
| NVMe BTRFS | `20b5fa7e-d8c0-4035-ae45-f80263073a96` | nvme0n1p2, nvme1n1p2 |
| SSD BTRFS | `2638d087-0be1-436e-bfe4-8d6551ec02be` | sdb, sdc |
| HDD BTRFS | `8b66e847-4273-4e2a-ad53-b312b3b3ee6d` | sda, sdd |
| ESP primary | `129B-4CA4` | nvme0n1p3 |
| ESP backup | `7DE5-027D` | nvme1n1p3 |
| Swap 0 | `ddba4cee-f2b9-4820-96bf-46ac82c6e779` | nvme0n1p1 |
| Swap 1 | `1966b9f0-0828-4d99-9cb8-e5138032f67b` | nvme1n1p1 |
| zram swap | `08152df1-c7eb-4485-8eeb-19d4b1bade94` | zram0 |

### All Serials at a Glance

| Device | Serial | Model |
|--------|--------|-------|
| nvme0n1 | `204445805771` | WD Black SN850X 1TB |
| nvme1n1 | `20465F802394` | WD Black SN850X 1TB |
| sdb | `S5HVNA0N303556E` | Samsung 860 PRO 1TB |
| sdc | `S246NWAG500270V` | Samsung 850 EVO mSATA 1TB |
| sda | `ZXA0MHSK` | Seagate Exos X24 24TB |
| sdd | `ZXA0V0EY` | Seagate Exos X24 24TB |

### Essential Commands Cheat Sheet

```bash
# --- HEALTH CHECK ---
sudo btrfs device stats /              # NVMe errors
sudo btrfs device stats /opt           # SSD errors
sudo btrfs device stats /hddRaid1     # HDD errors
sudo btrfs filesystem show             # All arrays, device status
sudo smartctl -a /dev/nvme0n1          # NVMe SMART
sudo smartctl -a /dev/sda              # HDD SMART

# --- DEGRADED OPERATIONS ---
sudo btrfs filesystem show             # Find "missing" device
mount -o degraded ...                  # Mount with one drive missing

# --- REPLACEMENT ---
sudo wipefs -a /dev/sdX                         # Clean new drive
sudo btrfs replace start <devid> /dev/sdX /mp   # Start replacement
sudo btrfs replace status /mountpoint            # Check progress

# --- PARTITION CLONING (NVMe only) ---
sudo sfdisk -d /dev/nvme0n1 | sudo sfdisk /dev/nvme1n1  # Clone layout
sudo mkswap /dev/nvmeXn1p1                               # Create swap
sudo mkfs.vfat -F32 /dev/nvmeXn1p3                       # Create ESP

# --- ESP MANAGEMENT ---
sudo /usr/local/bin/esp-sync.sh        # Manual ESP sync
efibootmgr -v                          # View boot entries
sudo efibootmgr --create --disk /dev/nvmeXn1 --part 3 \
  --loader '\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI' \
  --label "Linux Boot Manager" --unicode

# --- VERIFICATION ---
sudo btrfs scrub start -B /mountpoint  # Full integrity check
sudo snapper list-configs              # Verify snapper configs
```

### Replacement Drive Specifications

| Array | Required Spec | Minimum Size | Interface |
|-------|--------------|--------------|-----------|
| NVMe | PCIe Gen 4 NVMe M.2 2280 | 1 TB (931.5G) | M.2 NVMe |
| SSD | 2.5" or mSATA SATA III SSD | 1 TB (931.51G) | SATA III |
| HDD | 3.5" SATA III 7200 RPM | 24 TB (21.83 TiB) | SATA III |

**Exact replacement models** (for identical hardware):
- NVMe: WD Black SN850X 1TB (WDS100T1X0E-00AFY0)
- SSD (sdb): Samsung 860 PRO 1TB
- SSD (sdc): Samsung 850 EVO mSATA 1TB
- HDD: Seagate Exos X24 24TB (ST24000DM001-3Y7103)

---

## 10. Offline Backup Plan

A comprehensive offline backup strategy is documented separately in [`OFFLINE-BACKUP-PLAN.md`](OFFLINE-BACKUP-PLAN.md).

**Summary**:
- **Hardware**: TerraMaster D6-320 (6-bay USB 3.2 Gen2 JBOD) + 6x Seagate ST2000DM008 2TB
- **Software**: btrbk 0.32.6 + mbuffer (installed)
- **Irreplaceable data**: ~1 TiB (NVMe subvolumes, SSD /opt + /srv, ClaudeCodeProjects, audiobook sources)
- **Strategy**: JBOD with mirrored drive pairs (Drives 1+3, 2+4) + 2 cold spares
- **Not backed up**: VMs (recreatable), converted audiobooks (re-derivable), Steam/AI models/ISOs (re-downloadable), snapper snapshots (btrbk manages its own retention)
- **Status**: Active -- DAS purchased and operational

---

*Document generated: 2026-02-01 from live system data. All UUIDs, serials, and partition layouts verified against running system.*
