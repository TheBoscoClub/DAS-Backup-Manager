> **Note**: This is the author's specific CachyOS system. See [STORAGE-ARCHITECTURE-AND-RECOVERY.md](../STORAGE-ARCHITECTURE-AND-RECOVERY.md) for the generic guide.

# Storage Architecture & Emergency Recovery Guide

> **System**: CachyOS (Arch-based) on ASUS ROG Crosshair VIII **Hero**, BIOS 5601 (2026-08-18)
> **Boot**: systemd-boot (NOT GRUB)
> **Filesystem**: BTRFS on all arrays, RAID-1 mirrors
> **Last verified**: 2026-08-31 (boot/ESP facts re-verified after the board swap)
> **HDD RAID-1 balance**: COMPLETE (all data RAID-1 as of 2026-04-06)
>
> **Board swap, 2026-08-31**: the motherboard was replaced (Crosshair VIII **Dark
> Hero** → Crosshair VIII **Hero**). A board swap wipes UEFI NVRAM, so **every
> boot-entry number in this document changed**, and the named `Linux Boot Manager`
> entries this file used to describe no longer exist — the new firmware
> auto-created four generic `UEFI OS` entries instead. Device letters for the
> USB-attached DAS drives were also re-enumerated.
>
> **Boot entries are therefore documented by PARTUUID and LABEL below, not by
> entry number.** Entry numbers are firmware-assigned and are re-issued on any
> NVRAM reset; a recovery procedure that quotes one is wrong the moment the
> NVRAM is cleared — which is exactly when the procedure gets used.

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
|  PARTUUID ca1c0553-...        PARTUUID cc7834c1-...                |
|  (BootCurrent)                 (fallback -- boot it by PARTUUID,   |
|                                 never by a remembered entry no.)   |
|                                                                     |
|  /boot/loader/entries:         Synced via /usr/local/bin/esp-sync  |
|   +- linux-cachyos.conf       Triggered by pacman hook:            |
|   +- linux-cachyos-fallback   /etc/pacman.d/hooks/esp-mirror.hook  |
|   +- linux-cachyos-safe       per-file md5 compare + cp -a,        |
|   +- linux-cachyos-cli        NVMe-only, fail-closed (NOT rsync)   |
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
|                     HDD RAID-1 (BTRFS) -- 24TB x 2                 |
|                     sda + sdd (whole-disk)                          |
|            UUID: 8b66e847-4273-4e2a-ad53-b312b3b3ee6d               |
|                                                                     |
|  RAID-1 balance COMPLETE -- all data fully mirrored                 |
|  Data: RAID-1 (8.77 TiB total, 8.71 TiB used)                     |
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

#### UEFI Boot Entries — identify by PARTUUID, never by entry number

All four disk entries are **firmware auto-created generics**, every one of them
named `UEFI OS` and pointing at the removable-media fallback path
`\EFI\BOOT\BOOTX64.EFI`. The PARTUUID is the only field that distinguishes
them, and the only field that survives an NVRAM reset.

| GPT PARTUUID | Partition | LABEL | Role | EFI Path |
|--------------|-----------|-------|------|----------|
| `ca1c0553-72eb-4117-bac4-981927b721a6` | nvme0n1p3 | `EFI` | Primary ESP, mounted `/boot` | `\EFI\BOOT\BOOTX64.EFI` |
| `cc7834c1-a4c8-4090-b396-2eab7e9cf463` | nvme1n1p3 | `EFI-BACKUP` | Mirror ESP, mounted `/mnt/esp-backup` | `\EFI\BOOT\BOOTX64.EFI` |
| `fe640619-2c7b-457a-be77-61bc9aff4875` | 2TB bay 1, serial `ZK208Q77` | `RECOV-ESP-1` | **Independent recovery OS** | `\EFI\BOOT\BOOTX64.EFI` |
| `ef19ce6e-de5e-4623-bed0-8717749916b8` | 2TB bay 4, serial `ZFL41DNY` | `RECOV-ESP-4` | **Independent recovery OS** | `\EFI\BOOT\BOOTX64.EFI` |

**Never delete the last three entries.** The mirror entry is the failover if
`nvme0n1` dies; the two `RECOV-ESP-*` entries are how the standalone recovery
systems are booted. They look like firmware clutter and are not.

Resolve the current numbering — do this rather than trusting any number written
down here or anywhere else:

```bash
sudo efibootmgr -v
for u in ca1c0553-72eb-4117-bac4-981927b721a6 cc7834c1-a4c8-4090-b396-2eab7e9cf463 \
         fe640619-2c7b-457a-be77-61bc9aff4875 ef19ce6e-de5e-4623-bed0-8717749916b8; do
  printf '%s -> ' "$u"; blkid -t PARTUUID="$u" -o device
done
```

**Numbering as observed on 2026-08-31** (informational only — re-derive it with
the command above): `Boot0001` primary, `Boot0002` mirror, `Boot0003` bay 1
recovery, `Boot0004` bay 4 recovery; BootOrder `0001,0002,0003,0004,0005,0006,0007`;
BootCurrent `0001`.

**Known gap**: there is currently no *named* NVRAM entry for the primary ESP —
boot selection falls to firmware enumeration order, and two of the candidates
are USB-attached recovery disks. Creating a `Linux Boot Manager` entry resolved
by partition GUID (`bootctl install` on `/boot`) would pin this. That is a
bootloader change on the primary ESP and belongs to the CachyOS-Kernel project,
not to DAS-Backup-Manager — see `.claude/rules/esp-safety.md` for the boundary.

#### ESP Sync Chain

1. Pacman installs/upgrades kernel, initramfs, or bootloader
2. Pacman hook `/etc/pacman.d/hooks/esp-mirror.hook` fires (PostTransaction)
3. Calls `/usr/local/bin/esp-sync.sh`
4. The script walks `/boot` file-by-file, compares `md5sum` against the mirror,
   and `cp -a`s only what differs; it then removes files present on the mirror
   but absent from `/boot`. **It is not rsync** — earlier revisions of this
   document said `rsync -aHAXS --delete`, which understated the safety.
5. Both ESPs are now identical apart from `loader/random-seed`, which is
   per-ESP entropy and is deliberately never copied

**Three fail-closed guards run before a single byte is written** (`validate_device()`):

| Guard | Effect |
|-------|--------|
| Label ↔ mount cross-check | The device mounted at the path must be the same device `LABEL=EFI` / `LABEL=EFI-BACKUP` resolves to, else `REFUSING to sync` |
| **NVMe-only device class** | Any resolved device not matching `/dev/nvme*` aborts — this is what makes the USB-attached DAS ESPs structurally unreachable, regardless of what they are labelled |
| vfat + read-write check | Refuses if the mount is not a real read-write vfat ESP |

The device-class guard is the load-bearing one. Labels can be renamed by anyone
with `fatlabel`; a bus class cannot be renamed into existence. Note this matters
in practice: the DAS recovery ESPs were relabelled from `BACKUP-ESP` to
`RECOV-ESP-1` / `RECOV-ESP-4` at some point without any doc being updated, and
the sync mechanism was unaffected precisely because it never depended on their
label.

`loader/random-seed`, `loader/.#bootctl*` and `test-sync-trigger` are listed in
the script's `is_unique_file()` and are never synced in either direction.

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

### 2c. HDD RAID-1 -- Mass Storage

**Devices**: sda (Exos X24, 21.83 TiB, devid 1) + sdd (Exos X24, 21.83 TiB, devid 2)
**BTRFS UUID**: `8b66e847-4273-4e2a-ad53-b312b3b3ee6d`
**Profile**: Data RAID-1, Metadata RAID-1
**Current usage**: 8.71 TiB used / ~13.1 TiB free

> **CONVERSION HISTORY**: This array was originally RAID-0. A `btrfs balance` converting Data to RAID-1 completed between 2026-02-01 and 2026-04-06. All data, metadata, and system profiles are now fully RAID-1.

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

Check SATA drives. **Address them by `by-id` path, not by letter** — the
letters below drifted when the board was swapped (`ZXA0MHSK` was `/dev/sda`
in April and is `/dev/sdh` today), and they drift again on any USB
re-enumeration:
```bash
sudo smartctl -a /dev/disk/by-id/ata-Samsung_SSD_860_PRO_1TB_*      # 860 PRO
sudo smartctl -a /dev/disk/by-id/ata-Samsung_SSD_850_EVO_mSATA_1TB_*  # 850 EVO
sudo smartctl -a /dev/disk/by-id/ata-ST24000DM001-3Y7103_ZXA0MHSK   # Exos X24
sudo smartctl -a /dev/disk/by-id/ata-ST24000DM001-3Y7103_ZXA0V0EY   # Exos X24

# To see the current letter-to-serial mapping at any moment:
for d in /dev/sd?; do
  printf '%-9s ' "$d"; sudo smartctl -i "$d" | awk '/Serial Number:/{print $3}'
done
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
**Auto-recovery**: UEFI falls through to the mirror ESP on `nvme1n1p3`
(PARTUUID `cc7834c1-a4c8-4090-b396-2eab7e9cf463`, `LABEL=EFI-BACKUP`), which
holds identical ESP contents.

#### Step 1: Boot from backup NVMe

The boot order already includes an entry for `nvme1n1p3`. If the firmware does
not auto-fall-through:
1. Enter BIOS (DEL at POST)
2. Pick the entry for the **second NVMe**. Every disk entry is named `UEFI OS`,
   so the name cannot tell them apart — match on the partition, and if the menu
   is ambiguous, physically remove the failed `nvme0n1` so only one candidate
   remains. Do **not** pick either 2TB USB recovery disk here; those boot a
   different OS entirely.
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

# Set boot order. Do NOT copy a boot order from this document -- read the
# CURRENT entries and build the order from what is actually there:
sudo efibootmgr -v          # note the new entry's number, and the others
sudo efibootmgr -o <new>,<mirror>,<rest...>

# Whatever you do, keep the mirror ESP entry and BOTH RECOV-ESP-* entries in
# the order. They are the failover and the two recovery systems.
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
10. Re-register the mirror-ESP fallback entry (its number will be whatever
    the firmware assigns; do not expect a particular one):
    ```bash
    sudo efibootmgr --create --disk /dev/nvme1n1 --part 3 \
      --loader '\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI' \
      --label "Linux Boot Manager (NVMe1)" --unicode
    sudo efibootmgr -v    # confirm it exists and note its PARTUUID
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

RAID-1 balance is **COMPLETE** as of 2026-04-06. All data is fully mirrored. Standard recovery applies:

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
| SSD BTRFS | `2638d087-0be1-436e-bfe4-8d6551ec02be` | 860 PRO + 850 EVO mSATA (letters drift) |
| HDD BTRFS | `8b66e847-4273-4e2a-ad53-b312b3b3ee6d` | Exos X24 `ZXA0V0EY` + `ZXA0MHSK` (letters drift) |
| ESP recovery bay 1 | `6D15-0632` | 2TB `ZK208Q77`, `LABEL=RECOV-ESP-1` |
| ESP recovery bay 4 | `6CAB-B04D` | 2TB `ZFL41DNY`, `LABEL=RECOV-ESP-4` |
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
sudo smartctl -a /dev/disk/by-id/ata-ST24000DM001-3Y7103_ZXA0V0EY  # HDD SMART (by-id, not sdX)

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
- **Hardware**: TerraMaster D6-320 (6-bay USB 3.2 Gen2 JBOD) — 4 of 6 bays occupied (bays 3 and 6 empty)
- **Primary Backup (BTRFS RAID-1)**: 2x 22TB Exos (ST22000NM000C-3WC103) in bays 2 (`ZXA1R71M`, RMA replacement for failed `ZXA0LMAE` since 2026-05-15) and 5 (`ZXA1NYGZ`), single BTRFS filesystem `das-backup-22tb` UUID `b2dbe07d-40b9-422e-8ccf-ef4931c40457`. Mounted with `degraded` so single-leg failure does not interrupt backups, restores, or recovery.
- **Recovery Drives**: 2x 2TB Barracuda (independent, NOT a RAID pair) in bays 1 (`ZK208Q77`, `das-backup-system-recovery-A`) and 4 (`ZFL41DNY`, `das-backup-system-recovery-B`) — each can boot the system standalone via its own ESP
- **Internal SATA**: dasRaid0 (4x 2TB Barracuda RAID0, general storage) — moved from DAS 2026-04-06
- **Offline spares**: 1x 2TB Barracuda (ZFL416F6, cold spare for dasRaid0)
- **Software**: btrbk 0.32.6 + mbuffer (installed)
- **Irreplaceable data**: ~1 TiB (NVMe subvolumes, SSD /opt + /srv, ClaudeCodeProjects, audiobook sources)
- **Not backed up**: VMs (recreatable), converted audiobooks (re-derivable), Steam/AI models/ISOs (re-downloadable), snapper snapshots (btrbk manages its own retention)
- **Status**: Active — primary backup runs with live RAID-1 redundancy (added 2026-05-06)

---

*Document generated: 2026-02-01, updated 2026-05-06 (added second 22TB CMR drive in bay 5, das-backup-22tb converted to BTRFS RAID-1). All UUIDs, serials, and partition layouts verified against running system.*
