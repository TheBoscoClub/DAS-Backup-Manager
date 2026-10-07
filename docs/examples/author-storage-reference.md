> **Note**: This is the author's specific CachyOS system. See [STORAGE-ARCHITECTURE-AND-RECOVERY.md](../STORAGE-ARCHITECTURE-AND-RECOVERY.md) for the generic guide.

# Storage Architecture & Emergency Recovery Guide

> **System**: CachyOS (Arch-based) on ASUS ROG Crosshair VIII **Hero**, BIOS 5601 (2026-08-18)
> **Boot**: systemd-boot (NOT GRUB)
> **Filesystem**: BTRFS on all arrays — NVMe and HDD fully RAID-1; **the SATA SSD pool's data is RAID0 by design** (metadata RAID-1): no data redundancy, recovered from backup (§6)
> **Last verified**: 2026-10-02 (block devices, labels, UUIDs, PARTUUIDs, profiles, subvolumes, fstab, boot entries); 2026-08-31 for the boot/ESP facts after the board swap
> **HDD RAID-1 balance**: COMPLETE (all data RAID-1 as of 2026-04-06)
>
> **Board swap, 2026-08-29** (in service on BIOS 5601 from 2026-08-31): the
> motherboard was replaced (Crosshair VIII **Dark Hero** → Crosshair VIII **Hero**).
> A board swap wipes UEFI NVRAM, so **every boot-entry number in this document
> changed**, and the firmware auto-created four generic `UEFI OS` entries. Device
> letters for the SATA and USB-attached drives were re-enumerated, and **so were
> the NVMe names**: the boot drive (serial `204445805771`) was `nvme0n1` before and
> is `nvme1n1` on 2026-10-02. Every drive below is identified by serial; the kernel
> name shown is only what it was on 2026-10-02 — resolve it with
> `lsblk -d -o NAME,SERIAL` before running any command that names a device.
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

| Serial | Model | Size | Role | BTRFS devid | Kernel name on 2026-10-02 |
|--------|-------|------|------|-------------|---------------------------|
| 204445805771 | WD Black SN850X (WDS100T1X0E-00AFY0) | 1 TB (931.5G) | NVMe RAID-1, boot drive (ESP `EFI`) | 1 | nvme1n1 |
| 20465F802394 | WD Black SN850X (WDS100T1X0E-00AFY0) | 1 TB (931.5G) | NVMe RAID-1, mirror ESP (`EFI-BACKUP`) | 2 | nvme0n1 |
| S5HVNA0N303556E | Samsung SSD 860 PRO 1TB | 1 TB (953.9G) | SSD pool (`sata_pool`) | 1 | sde |
| S246NWAG500270V | Samsung SSD 850 EVO mSATA 1TB | 1 TB (931.5G) | SSD pool (`sata_pool`) | 2 | sdg |
| ZXA0MHSK | Seagate ST24000DM001-3Y7103 | 24 TB (21.83 TiB) | HDD RAID-1 | 1 | sdh |
| ZXA0V0EY | Seagate ST24000DM001-3Y7103 | 24 TB (21.83 TiB) | HDD RAID-1 | 2 | sdb |

The internal `dasRaid0` array (4 × 2 TB) and the USB-attached DAS backup drives are documented in [author-bay-mapping.md](author-bay-mapping.md).

### BTRFS Filesystem UUIDs

| Array | UUID | Label | Profiles (data / metadata) |
|-------|------|-------|----------------------------|
| NVMe RAID-1 | `20b5fa7e-d8c0-4035-ae45-f80263073a96` | (none) | RAID1 / RAID1 |
| SSD pool | `2638d087-0be1-436e-bfe4-8d6551ec02be` | `sata_pool` | **RAID0** / RAID1 |
| HDD RAID-1 | `8b66e847-4273-4e2a-ad53-b312b3b3ee6d` | (none) | RAID1 / RAID1 |
| dasRaid0 (internal SATA) | `d29fdda7-a1e5-4640-996e-2b78569cb65d` | `dasRaid0` | RAID0 / RAID1C3 |

Profiles read from `/sys/fs/btrfs/<uuid>/allocation/` on 2026-10-02.

### Array -> Subvolume -> Mount Point Diagram

```
+---------------------------------------------------------------------+
|                        NVMe RAID-1 (BTRFS)                          |
|  Partition 2 of 204445805771 (devid 1) + 20465F802394 (devid 2)     |
|  926G each   UUID: 20b5fa7e-d8c0-4035-ae45-f80263073a96             |
|                                                                     |
|  @ -> /              @home -> /home         @root -> /root          |
|  @log -> /var/log    @var-lib-audiobooks -> /var/lib/audiobooks     |
|  @etc-audiobooks -> /etc/audiobooks                                 |
|  @/@audiobooks-db -> /var/lib/audiobooks/db  (nested inside @)      |
|  @libvirt-images -> /var/lib/libvirt/images                         |
|                                                                     |
|  Also: @tmp, @var-tmp (unused -- /tmp, /var/tmp are tmpfs)          |
|  Snapper configs: see section 8e                                    |
+---------------------------------------------------------------------+

+---------------------------------------------------------------------+
|                     ESP Dual-Boot Architecture                      |
|                                                                     |
|  Partition 3 of 204445805771      Partition 3 of 20465F802394       |
|  (nvme1n1p3 on 2026-10-02)        (nvme0n1p3 on 2026-10-02)         |
|  LABEL=EFI   UUID: 129B-4CA4      LABEL=EFI-BACKUP  UUID: 7DE5-027D |
|  Mount: /boot (primary)           Mount: /mnt/esp-backup            |
|  PARTUUID ca1c0553-...            PARTUUID cc7834c1-...             |
|  (BootCurrent)                    (fallback -- boot it by PARTUUID, |
|                                    never by a remembered entry no.) |
|                                                                     |
|  /boot/loader/entries:            Synced via /usr/local/bin/esp-sync|
|   +- linux-cachyos.conf          Triggered by pacman hook:          |
|   +- linux-cachyos-fallback      /etc/pacman.d/hooks/esp-mirror.hook|
|   +- linux-cachyos-safe          per-file md5 compare + cp -a,      |
|   +- linux-cachyos-cli           NVMe-only, fail-closed (NOT rsync) |
+---------------------------------------------------------------------+

+---------------------------------------------------------------------+
|                SSD pool `sata_pool` (BTRFS) -- data RAID0           |
|  S5HVNA0N303556E (860 PRO) + S246NWAG500270V (850 EVO), whole-disk  |
|  UUID: 2638d087-0be1-436e-bfe4-8d6551ec02be                         |
|  Data RAID0 by design, metadata RAID1                               |
|                                                                     |
|  @opt -> /opt        @srv -> /srv          @cache -> /var/cache     |
|  @hibp -> ~/.local/share/hibp-checker                               |
|  @docker -> /var/lib/docker    @steam -> /srv/SteamLibrary          |
|  Nested: @srv/VirtualMachines, @srv/stremio-web, @cache/stremio     |
|  Snapper configs: see section 8e                                    |
+---------------------------------------------------------------------+

+---------------------------------------------------------------------+
|                  HDD RAID-1 (BTRFS) -- 24TB x 2                     |
|  ZXA0MHSK (devid 1) + ZXA0V0EY (devid 2), whole-disk                |
|  UUID: 8b66e847-4273-4e2a-ad53-b312b3b3ee6d                         |
|                                                                     |
|  RAID-1 for data, metadata and system -- fully mirrored             |
|  8.27 TiB of data (2026-10-02); 8.34 TiB allocated per device       |
|                                                                     |
|  Top-level subvolumes:                                              |
|  +- ClaudeCodeProjects -> /hddRaid1/ClaudeCodeProjects              |
|  |  (24 project subvolumes, see section 2c)                         |
|  +- Audiobooks -> /hddRaid1/Audiobooks                              |
|  +- SteamLibrary -> /hddRaid1/SteamLibrary                          |
|  +- SteamLibrary-local -> ~/.local/share/Steam                      |
|  +- ISOs -> /hddRaid1/ISOs                                          |
|  +- bosco-media (nested: bosco-media/video)                         |
|  +- claude-config -> ~/.claude                                      |
|  +- coredumps -> /var/lib/systemd/coredump                          |
+---------------------------------------------------------------------+

+---------------------------------------------------------------------+
|                         Swap Configuration                          |
|                                                                     |
|  zram0: 125.7G -- the only active swap (UUID regenerated each boot) |
|  Partition 1 of each NVMe: 4G swap, present but NOT in fstab, unused|
|    204445805771: UUID ddba4cee-f2b9-4820-96bf-46ac82c6e779 (swap2)  |
|    20465F802394: UUID 1966b9f0-0828-4d99-9cb8-e5138032f67b          |
|                                                                     |
|  tmpfs: /tmp (64G), /var/tmp (32G) -- NVMe wear reduction           |
+---------------------------------------------------------------------+
```

---

## 2. RAID Array Reference

### 2a. NVMe RAID-1 -- Boot & Root (Most Critical)

**Devices**: partition 2 of serial `204445805771` (926G, devid 1; `nvme1n1p2` on 2026-10-02) + partition 2 of serial `20465F802394` (926G, devid 2; `nvme0n1p2` on 2026-10-02)
**BTRFS UUID**: `20b5fa7e-d8c0-4035-ae45-f80263073a96`
**Profile**: Data RAID-1, Metadata RAID-1
**Converted from RAID-0**: 2026-01-31
**Current usage** (2026-10-02): 306.91 GiB of data; 353.62 GiB of each 926.01 GiB device allocated

#### Partition Layout (identical on both drives)

```
Partition   Start Sector   Size    Type                    Purpose
--------------------------------------------------------------------
p3          2048           1.5G    C12A7328 (EFI System)   ESP / systemd-boot
p1          3145728        4G      0657FD6D (Linux Swap)   Swap partition
p2          11534336       926G    0FC63DAF (Linux FS)     BTRFS RAID-1 root
```

#### Partition UUIDs (GPT PARTUUIDs)

| Partition | `204445805771` (boot drive) PARTUUID | `20465F802394` (mirror drive) PARTUUID |
|-----------|--------------------------------------|----------------------------------------|
| p1 (swap) | `DA9EB6F7-6C4F-4D54-880D-337FE5A45171` | `94F34602-51A2-444C-B930-A265ADA6BFDF` |
| p2 (BTRFS)| `ADFDF354-30D4-47F8-A98C-C0BB689E0EF8` | `37E61C6B-9448-46E9-917D-77D73DA28A4B` |
| p3 (ESP)  | `CA1C0553-72EB-4117-BAC4-981927B721A6` | `CC7834C1-A4C8-4090-B396-2EAB7E9CF463` |

#### UEFI Boot Entries — identify by PARTUUID, never by entry number

Four of the disk entries are **firmware auto-created generics**, every one of them
named `UEFI OS` and pointing at the removable-media fallback path
`\EFI\BOOT\BOOTX64.EFI`. The PARTUUID is the only field that distinguishes
them, and the only field that survives an NVRAM reset. Since the 2026-08-31
observation the primary ESP has also gained two named entries, `Linux Boot Manager`
(`\EFI\systemd\systemd-bootx64.efi`) and `Fallback Linux Boot Manager`
(`\EFI\systemd\systemd-boot-fallbackx64.efi`), both on PARTUUID `ca1c0553-…`.

| GPT PARTUUID | Partition | LABEL | Role | EFI Path |
|--------------|-----------|-------|------|----------|
| `ca1c0553-72eb-4117-bac4-981927b721a6` | NVMe `204445805771` p3 | `EFI` | Primary ESP, mounted `/boot` | `\EFI\BOOT\BOOTX64.EFI` |
| `cc7834c1-a4c8-4090-b396-2eab7e9cf463` | NVMe `20465F802394` p3 | `EFI-BACKUP` | Mirror ESP, mounted `/mnt/esp-backup` | `\EFI\BOOT\BOOTX64.EFI` |
| `fe640619-2c7b-457a-be77-61bc9aff4875` | 2TB bay 1, serial `ZK208Q77` | `RECOV-ESP-1` | **Independent recovery OS** | `\EFI\BOOT\BOOTX64.EFI` |
| `ef19ce6e-de5e-4623-bed0-8717749916b8` | 2TB bay 4, serial `ZFL41DNY` | `RECOV-ESP-4` | **Independent recovery OS** | `\EFI\BOOT\BOOTX64.EFI` |

**Never delete the last three entries.** The mirror entry is the failover if
the boot drive (`204445805771`) dies; the two `RECOV-ESP-*` entries are how the standalone recovery
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

**Observed on 2026-10-02** (informational only): the same four `UEFI OS` entries
keep those numbers, `Boot0000` is `Linux Boot Manager` and `Boot0005` is
`Fallback Linux Boot Manager` (both primary ESP); BootOrder
`0001,0002,0003,0004,0000,0005,0006,0007,0008`; BootCurrent `0001`.

**Former gap, closed by 2026-10-02**: on 2026-08-31 there was no *named* NVRAM
entry for the primary ESP. `Linux Boot Manager` now exists, but BootOrder still
lists it after the four `UEFI OS` entries, two of which are USB-attached recovery
disks; the first entry (`UEFI OS` on the primary ESP) is what boots. Reordering is
a bootloader change on the primary ESP and belongs to the CachyOS-Kernel project,
not to DAS-Backup-Manager — see `.claude/rules/esp-safety.md` for the boundary.

#### ESP Sync Chain

1. A pacman transaction installs, upgrades or removes a file the hook watches: a
   kernel image (`usr/lib/modules/*/vmlinuz`), anything under `usr/lib/initcpio/`,
   or anything a package puts under `boot/`. A systemd-boot update does not fire
   it: `bootctl update` writes `/boot/EFI` outside pacman's file list, so the
   mirror catches up at the next transaction that does, or at a manual
   `sudo /usr/local/bin/esp-sync.sh`
2. Pacman hook `/etc/pacman.d/hooks/esp-mirror.hook` fires (PostTransaction)
3. Calls `/usr/local/bin/esp-sync.sh`
4. The script walks `/boot` file-by-file, compares `md5sum` against the mirror,
   and `cp -a`s only what differs; it then removes files present on the mirror
   but absent from `/boot`. **It is not rsync** — earlier revisions of this
   document said `rsync -aHAXS --delete`, which understated the safety.
5. Both ESPs are now identical apart from the per-ESP files listed below
   (`loader/random-seed` is per-ESP entropy and is deliberately never copied)

**Three fail-closed guards run before a single byte is written** (`validate_device()`):

| Guard | Effect |
|-------|--------|
| Label ↔ mount cross-check | The device mounted at the path must be the same device `LABEL=EFI` / `LABEL=EFI-BACKUP` resolves to, else `REFUSING to sync` |
| **NVMe-only device class** | Any resolved device not matching `/dev/nvme*` aborts — this is what makes the USB-attached DAS ESPs structurally unreachable, regardless of what they are labelled |
| vfat check | Refuses if the filesystem at the path is not vfat |

The device-class guard is the load-bearing one. Labels can be renamed by anyone
with `fatlabel`; a bus class cannot be renamed into existence. Note this matters
in practice: the DAS recovery ESPs were relabelled from `BACKUP-ESP` to
`RECOV-ESP-1` / `RECOV-ESP-4` at some point without any doc being updated, and
the sync mechanism was unaffected precisely because it never depended on their
label.

`loader/random-seed`, `loader/.#bootctl*` and `test-sync-trigger` are listed in
the script's `is_unique_file()` and are never synced in either direction.

#### Boot Entries (systemd-boot)

*Read from the running system on 2026-10-07 (`sudo cat /boot/loader/entries/*.conf`; `/boot` is readable by root only).*

**Default** (`linux-cachyos.conf`) -- no `degraded`, so it cannot mount the root with one NVMe missing:
```
title Linux CachyOS
options root=UUID=20b5fa7e-d8c0-4035-ae45-f80263073a96 rw rootflags=subvol=/@ zswap.enabled=0 nowatchdog mitigations=off pci=realloc amdgpu.gpu_recovery=1 amdgpu.runpm=0 amdgpu.ppfeaturemask=0xffffffff amdgpu.mcbp=0 mem_sleep_default=s2idle quiet splash
linux /vmlinuz-linux-cachyos
initrd amd-ucode.img
initrd /initramfs-linux-cachyos.img
```

**Safe Mode** (`linux-cachyos-safe.conf`) -- for degraded boot:
```
title   CachyOS (Safe Mode)
sort-key 02
options root=UUID=20b5fa7e-d8c0-4035-ae45-f80263073a96 rw rootflags=subvol=/@,degraded btrfs.device_scan_wait=1 nomodeset
linux   /vmlinuz-linux-cachyos
initrd  /amd-ucode.img
initrd  /initramfs-linux-cachyos.img
```

**CLI Only** (`linux-cachyos-cli.conf`) -- no GUI, mounts degraded:
```
title CachyOS (CLI Only)
sort-key 04
options root=UUID=20b5fa7e-d8c0-4035-ae45-f80263073a96 rw rootflags=subvol=/@,degraded zswap.enabled=0 nowatchdog mitigations=off workqueue.power_efficient=0 amdgpu.gpu_recovery=1 amdgpu.reset_method=-1 mem_sleep_default=s2idle systemd.unit=multi-user.target
linux /vmlinuz-linux-cachyos
initrd /amd-ucode.img
initrd /initramfs-linux-cachyos.img
```

### 2b. SSD Pool (`sata_pool`) -- Services & VMs

**Devices**: serial `S5HVNA0N303556E` (Samsung 860 PRO, 953.87G, devid 1; `sde` on 2026-10-02) + serial `S246NWAG500270V` (Samsung 850 EVO mSATA, 931.51G, devid 2; `sdg` on 2026-10-02), whole-disk
**BTRFS UUID**: `2638d087-0be1-436e-bfe4-8d6551ec02be`
**Label**: `sata_pool`
**Profile**: **Data RAID0**, Metadata RAID-1, System RAID-1 (`/sys/fs/btrfs/<uuid>/allocation/`, 2026-10-02)
**Current usage** (2026-10-02): 1007.53 GiB of data; 520.03 GiB of each device allocated

**By design** (storage policy confirmed 2026-10-02): the data is striped, not mirrored. Nothing the boot needs is on this pool, every mount of it is `nofail`, and everything on it except `/var/cache` is in the nightly backup, so the machine boots without it and the recovery is replace the drive, recreate the pool, restore from the DAS backups (§6). Losing **either** SSD loses the data on this pool; the pool does not keep running on the other one. A mirror is not the plan and would not fit anyway (about 1007.53 GiB of data against the 931.51 GiB smaller SSD).

#### Subvolumes

| Subvolume | Mount Point | Options (fstab) | Purpose | Backed up as |
|-----------|-------------|-----------------|---------|--------------|
| @opt | /opt | ssd,compress=zstd:3 | Installed software | `ssd/opt.<TS>` (all targets) |
| @srv | /srv | ssd,compress=zstd:1 | Server data | `ssd/srv.<TS>` (all targets) |
| @srv/VirtualMachines | /srv/VirtualMachines (nested in @srv) | inherits /srv | libvirt QCOW2 images | `ssd-vm/srv-VirtualMachines.<TS>` (primary only) |
| @srv/stremio-web | /srv/stremio-web (nested in @srv) | inherits /srv | Stremio web | `ssd/srv-stremio-web.<TS>` (all targets) |
| @steam | /srv/SteamLibrary | ssd,compress=zstd:1 | Steam games | `ssd-steam/steam.<TS>` (primary only) |
| @docker | /var/lib/docker | ssd,compress=zstd:3 | Docker data | `ssd/docker.<TS>` (all targets) |
| @hibp | ~/.local/share/hibp-checker | ssd,compress=zstd:1 | HIBP password data | `ssd/hibp.<TS>` (all targets) |
| @cache | /var/cache | ssd,nodatacow | Package cache | not backed up (excluded) |
| @cache/stremio | (nested in @cache) | inherits /var/cache | Stremio cache | not backed up (excluded with `@cache`) |

### 2c. HDD RAID-1 -- Mass Storage

**Devices**: serial `ZXA0MHSK` (ST24000DM001, 21.83 TiB, devid 1; `sdh` on 2026-10-02) + serial `ZXA0V0EY` (ST24000DM001, 21.83 TiB, devid 2; `sdb` on 2026-10-02)
**BTRFS UUID**: `8b66e847-4273-4e2a-ad53-b312b3b3ee6d`
**Profile**: Data RAID-1, Metadata RAID-1
**Current usage** (2026-10-02): 8.27 TiB of data; 8.34 TiB of each 21.83 TiB device allocated

> **CONVERSION HISTORY**: This array was originally RAID-0. A `btrfs balance` converting Data to RAID-1 completed between 2026-02-01 and 2026-04-06. All data, metadata, and system profiles are now fully RAID-1.

#### Top-Level Subvolumes (non-snapshot, 2026-10-02)

| Subvolume | Mount Point | compress (fstab) | Notes |
|-----------|-------------|------------------|-------|
| ClaudeCodeProjects | /hddRaid1/ClaudeCodeProjects | zstd:3 | Parent for all Claude projects |
| Audiobooks | /hddRaid1/Audiobooks | zstd:3 | Audiobook files |
| SteamLibrary | /hddRaid1/SteamLibrary | zstd:3 | Steam games (secondary) |
| SteamLibrary-local | ~/.local/share/Steam | zstd:3 | Steam games (primary) |
| ISOs | /hddRaid1/ISOs | zstd:3 | ISO images |
| bosco-media | (under /hddRaid1) | zstd:3 (top-level mount) | Media; `bosco-media/video` is a nested subvolume |
| claude-config | ~/.claude | zstd:3 | Claude config, presented by a systemd mount (not fstab) |
| coredumps | /var/lib/systemd/coredump | no | systemd-coredump; not backed up (excluded) |

The `ai-models-*` subvolumes and `VirtualMachines` listed in earlier revisions no longer exist on this filesystem.

#### Project Subvolumes (under ClaudeCodeProjects, 2026-10-07)

Each is an independent BTRFS subvolume with its own `config.toml` entry (source `hdd-projects`, all three targets); a project created later is adopted into that source by the next backup run. Only some have a Snapper config (§8e):

Audiobook-Manager, CachyOS-Kernel, cachyos-sentinel, ccp, claude-code-streaming-feature, claude-cowork-desktop-maintenance, claude-test-skill, cloudflare-manager, cloud-gpu-toolkit, DAS-Backup-Manager, General-Chat, github-maintenance, gstack, hibp-project, libvirt-vm-manager, mcp-workspace, powershell-scripts, scx-autoswitch, steam-sam-optimizer, stremio-manager, the_bosco_club, the-last-shave, website-dev, .repo-templates

---

## 3. Failure Detection

### 3a. SMART Monitoring

Check NVMe drives:
```bash
sudo smartctl -a /dev/disk/by-id/nvme-WDS100T1X0E-00AFY0_204445805771   # boot drive
sudo smartctl -a /dev/disk/by-id/nvme-WDS100T1X0E-00AFY0_20465F802394   # mirror drive
```

Check SATA drives. **Address them by `by-id` path, not by letter** — the
letters below drifted when the board was swapped (`ZXA0MHSK` was `/dev/sda`
in April and is `/dev/sdh` today), and they drift again on any USB
re-enumeration:
```bash
sudo smartctl -a /dev/disk/by-id/ata-Samsung_SSD_860_PRO_1TB_*      # 860 PRO
sudo smartctl -a /dev/disk/by-id/ata-Samsung_SSD_850_EVO_mSATA_1TB_*  # 850 EVO
sudo smartctl -a /dev/disk/by-id/ata-ST24000DM001-3Y7103_ZXA0MHSK   # 24 TB HDD
sudo smartctl -a /dev/disk/by-id/ata-ST24000DM001-3Y7103_ZXA0V0EY   # 24 TB HDD

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
sudo btrfs device stats /opt        # SSD pool (data RAID0)
sudo btrfs device stats /hddRaid1   # HDD RAID-1

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
  | NVMe `204445805771` or `20465F802394` | WD Black SN850X 1TB NVMe M.2 2280 (WDS100T1X0E) | 931.5G (1 TB) |
  | SSD `S5HVNA0N303556E` (860 PRO) | Any 1TB SATA SSD | 931.51G (1 TB) |
  | SSD `S246NWAG500270V` (850 EVO) | Any 1TB SATA SSD | 931.51G (1 TB) |
  | HDD `ZXA0MHSK` or `ZXA0V0EY` | Seagate ST24000DM001 (24 TB) | 21.83 TiB (24 TB) |

---

## 5. Recovery: NVMe Failure

**Resolve the drives by serial first, every time.** The kernel names swap between
boots (on 2026-10-02 the boot drive `204445805771` was `nvme1n1`; before the board
swap it was `nvme0n1`), and a replacement drive's name is unpredictable. Every
command below uses these variables, never a literal name:

```bash
lsblk -d -o NAME,SIZE,MODEL,SERIAL /dev/nvme?n1
SURV=/dev/disk/by-id/nvme-WDS100T1X0E-00AFY0_<surviving-serial>   # the drive still in the array
NEW=/dev/nvmeXn1         # the replacement: no partitions, serial not in §1
```

### 5a. The Boot Drive Fails (serial `204445805771`, primary ESP `EFI`)

**Impact**: System loses primary ESP (/boot) and one leg of root RAID-1.
**Auto-recovery**: UEFI falls through to the mirror ESP on drive `20465F802394`
(PARTUUID `cc7834c1-a4c8-4090-b396-2eab7e9cf463`, `LABEL=EFI-BACKUP`), which
holds identical ESP contents. Note that `/etc/fstab` mounts `/boot` by `LABEL=EFI`
without `nofail`, so with that ESP gone the boot can stop in emergency mode; from
there, add `nofail` to the `/boot` line and continue (do not comment the line out:
Step 8 mounts the new ESP through it).

#### Step 1: Boot from the mirror NVMe

The boot order already includes an entry for the mirror ESP. If the firmware does
not auto-fall-through:
1. Enter BIOS (DEL at POST)
2. Pick the entry for the **mirror NVMe** (`20465F802394`). Every disk entry is named `UEFI OS`,
   so the name cannot tell them apart — match on the partition, and if the menu
   is ambiguous, physically remove the failed drive so only one candidate
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

1. Power off, install the new NVMe in the slot the failed drive came out of
2. Boot from the mirror drive again (may need BIOS selection), then set `SURV` and `NEW` as above

#### Step 4: Clone partition table

```bash
# Dump partition layout from surviving drive and apply to new drive.
# sfdisk -d carries the disk GUID (label-id:) and every partition's uuid=, and
# sfdisk applies them: drop both so the new drive gets fresh PARTUUIDs instead of
# duplicating the survivor's (PARTUUIDs identify the ESPs and their firmware entries)
sudo sfdisk -d "$SURV" | sed '/^label-id/d; s/, *uuid=[^,]*//' | sudo sfdisk "$NEW"

# Verify
sudo sfdisk -l "$NEW"
```

#### Step 5: Create swap partition (optional)

The NVMe swap partitions are not in fstab and are unused (zram is the swap), so this only keeps the layout identical:

```bash
sudo mkswap -L swap2 "${NEW}p1"
```

#### Step 6: Create ESP

The label is what matters: fstab mounts `/boot` by `LABEL=EFI`, and `esp-sync.sh`
finds both ESPs by label.

```bash
sudo mkfs.vfat -F32 -n EFI "${NEW}p3"
```

#### Step 7: Replace the failed BTRFS device

```bash
# Find the devid of the missing device
sudo btrfs filesystem show /
# "*** Some devices missing" means one is gone; its devid is on the line
# that ends in MISSING -- note that number
# (the boot drive 204445805771 is devid 1)

# Start replacement
sudo btrfs replace start <missing-devid> "${NEW}p2" / -B
# -B runs in foreground (recommended for monitoring)
# This will take ~15-30 minutes for ~310 GiB of data

# Monitor progress if running without -B:
sudo btrfs replace status /

# Anything written while degraded is in `single` chunks: convert back
sudo btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /
```

#### Step 8: Populate the new primary ESP

Do **not** hand-copy files from the mirror ESP: ESP contents are only ever moved by
`esp-sync.sh`, and that works in one direction only, `/boot` → `/mnt/esp-backup`. Reinstall instead
(bootloader work on the primary ESP is CachyOS-Kernel's — see
`.claude/rules/esp-safety.md`):

The order matters because the mirror can lose files here. `esp-mirror.hook` fires
after any pacman transaction that touches a kernel or `boot/*`, and `esp-sync.sh`
**deletes from the mirror every file that `/boot` lacks**. Within one transaction
pacman runs hooks in filename order, so `esp-mirror.hook` syncs *before*
`sdboot-kernel-update.hook` writes the generated entries: those come back with the
last sync below. Anything nothing regenerates must be on `/boot` before the
transaction, or the mirror you just booted from loses it for good. That means
`amd-ucode.img` (owned by `amd-ucode`, not the kernel package, and named in the
`initrd` line of every entry except the fallback) and the hand-written `linux-cachyos-safe.conf` and
`linux-cachyos-cli.conf` (§2a, Boot Entries).

```bash
sudo mount /boot                      # LABEL=EFI from fstab — now the new ESP
sudo bootctl install                  # systemd-boot + its NVRAM entry
# Recreate the hand-written entries from §2a — sdboot-manage does not generate them:
sudoedit /boot/loader/entries/linux-cachyos-safe.conf
sudoedit /boot/loader/entries/linux-cachyos-cli.conf
# Inventory what the mirror holds that /boot does not yet. Every "would remove"
# line must be a file the transaction below writes (vmlinuz-*, initramfs-*,
# amd-ucode.img) or a generated linux-cachyos*.conf entry; recreate anything else first.
sudo /usr/local/bin/esp-sync.sh --dry-run
# ONE transaction, every package that owns a file on the ESP (amd-ucode owns
# /boot/amd-ucode.img; list them: pacman -Ql | grep ' /boot/' | awk '{print $1}' | sort -u),
# plus every installed kernel. The esp-mirror hook fires after the transaction.
sudo pacman -S amd-ucode linux-cachyos   # add linux-cachyos-lts etc. if installed
sudo sdboot-manage gen                # the kernel hook already ran autogen; harmless
sudo /usr/local/bin/esp-sync.sh --dry-run   # must print no "would remove" line
sudo /usr/local/bin/esp-sync.sh
```

#### Step 9: Check the UEFI boot entries

```bash
# Do NOT copy a boot order from this document -- read the CURRENT entries
# (bootctl install creates "Linux Boot Manager" for the new ESP):
sudo efibootmgr -v          # note the new entry's number, and the others
sudo efibootmgr -o <new>,<mirror>,<rest...>

# Whatever you do, keep the mirror ESP entry and BOTH RECOV-ESP-* entries in
# the order. They are the failover and the two recovery systems.
```

#### Step 10: fstab

No change is needed: `/boot` and `/mnt/esp-backup` are mounted by `LABEL=`, and
root by the BTRFS filesystem UUID, which `btrfs replace` keeps.

#### Step 11: Verify ESP sync

```bash
# Step 8 already synced; a re-run copies only what changed since
sudo /usr/local/bin/esp-sync.sh

# Verify both ESPs are identical
sudo bash -c 'diff <(cd /boot && find . -type f -exec md5sum {} + | sort -k2) \
                   <(cd /mnt/esp-backup && find . -type f -exec md5sum {} + | sort -k2)'
# Only the files is_unique_file() exempts may differ: loader/random-seed (per-ESP
# entropy), loader/.#bootctl* (bootctl temp files) and test-sync-trigger
```

### 5b. The Mirror Drive Fails (serial `20465F802394`, mirror ESP `EFI-BACKUP`)

**Impact**: System boots normally from the boot drive. Lost: backup ESP + one RAID-1 leg.
**Urgency**: Medium -- system is fully functional but unprotected.

#### Steps

1. Boot normally (the boot drive `204445805771` holds the primary ESP)
2. Verify degraded: `sudo btrfs filesystem show /`
3. Install replacement NVMe, then set `SURV` and `NEW` as above
4. Clone partition table:
   ```bash
   # fresh GUIDs, not the survivor's — see §5a Step 4
   sudo sfdisk -d "$SURV" | sed '/^label-id/d; s/, *uuid=[^,]*//' | sudo sfdisk "$NEW"
   ```
5. Create swap (optional, unused): `sudo mkswap "${NEW}p1"`
6. Create ESP **with the label** `esp-sync.sh` and fstab look for:
   `sudo mkfs.vfat -F32 -n EFI-BACKUP "${NEW}p3"`
7. Replace BTRFS device (the mirror drive is devid 2), then convert any `single` chunks:
   ```bash
   sudo btrfs replace start <missing-devid> "${NEW}p2" / -B
   sudo btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /
   ```
8. Mount backup ESP and sync:
   ```bash
   sudo mount /mnt/esp-backup       # LABEL=EFI-BACKUP from fstab
   sudo /usr/local/bin/esp-sync.sh
   ```
9. fstab needs no change (it mounts `/mnt/esp-backup` by `LABEL=EFI-BACKUP`).
10. Re-register the mirror-ESP fallback entry (its number will be whatever
    the firmware assigns; do not expect a particular one):
    ```bash
    sudo efibootmgr --create --disk "$NEW" --part 3 \
      --loader '\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI' \
      --label "Linux Boot Manager (NVMe mirror)" --unicode
    sudo efibootmgr -v    # confirm it exists and note its PARTUUID
    ```

---

## 6. Recovery: SSD Failure

**Devices**: serial `S5HVNA0N303556E` (Samsung 860 PRO, devid 1) + serial `S246NWAG500270V` (Samsung 850 EVO mSATA, devid 2)
**Impact**: the pool's **data is RAID0** (§2b), so losing either SSD loses /opt, /srv
(with its nested VirtualMachines and stremio-web), /srv/SteamLibrary, /var/lib/docker,
/var/cache and the HIBP data. The pool cannot be mounted from one drive and `btrfs replace`
cannot rebuild it; it has to be recreated and restored from the DAS backups. Every
SSD fstab entry carries `nofail`, so the system itself still boots.

> **Size note**: the 860 PRO is 953.87G, the 850 EVO 931.51G. Replacement must be >= 931.51G (1 TB class).

### Either Drive Fails

#### Step 1: Identify which drive failed

```bash
sudo btrfs filesystem show 2638d087-0be1-436e-bfe4-8d6551ec02be
# Shows which devid is missing
lsblk -d -o NAME,SIZE,MODEL,SERIAL     # which serial is gone
```

#### Step 2: Install replacement

Power off, install new SATA SSD.

#### Step 3: Recreate the pool

```bash
# By serial, never by letter
SSD1=/dev/disk/by-id/ata-<model>_<surviving-serial>
SSD2=/dev/disk/by-id/ata-<model>_<new-serial>
sudo wipefs -a "$SSD1" "$SSD2"
sudo mkfs.btrfs -L sata_pool -d raid0 -m raid1 "$SSD1" "$SSD2"
# A new filesystem gets a new UUID: update every UUID=2638d087-... line in
# /etc/fstab and the `device = "UUID=..."` of the ssd, ssd-steam and ssd-vm
# sources in /etc/das-backup/config.toml.
sudo mount /.btrfs-ssd                       # top level (subvolid=5) from fstab
sudo mkdir /.btrfs-ssd/.btrbk-snapshots      # the sources' snapshot_dir
```

#### Step 4: Restore the subvolumes from the primary DAS target

The latest copies are on the primary target (`/mnt/backup-22tb`): `ssd/` (`opt`, `srv`,
`srv-stremio-web`, `docker`, `hibp`), `ssd-steam/steam` and `ssd-vm/srv-VirtualMachines`,
plus `ssd-adopted/` if a backup run adopted a new subvolume there. `@cache` was never
backed up (excluded) and is simply recreated empty. For each subvolume: `btrfs send`
the newest snapshot, `btrfs receive` it onto the new pool, then take a **writable**
snapshot of it under the original name (`@opt`, `@srv`, …) — never
`btrfs property set … ro false` on a received snapshot. Restore `@srv` before the
subvolumes nested in it (`@srv/VirtualMachines` and `@srv/stremio-web` in `btrbk.conf`
on 2026-10-02). `btrfs send` does not descend into nested subvolumes, so the restored
`@srv` holds an empty directory where each sat, and a snapshot made onto an existing
directory lands *inside* it, not at the path fstab expects. Remove the empty placeholders
first: `sudo rmdir /.btrfs-ssd/@srv/VirtualMachines /.btrfs-ssd/@srv/stremio-web`, then
snapshot the nested ones into place.

#### Step 5: Verify

```bash
sudo btrfs filesystem show /opt     # Both devices present
sudo btrfs device stats /opt        # All zeros
sudo btrfs scrub start -B /opt      # Full integrity check
```

---

## 7. Recovery: HDD Failure

**Devices**: serial `ZXA0MHSK` (ST24000DM001, devid 1) + serial `ZXA0V0EY` (ST24000DM001, devid 2)
**Mount**: /hddRaid1 and all subvolumes

RAID-1 balance is **COMPLETE** as of 2026-04-06. All data is fully mirrored. Standard recovery applies:

```bash
# 1. Identify failed drive
sudo btrfs filesystem show /hddRaid1
lsblk -d -o NAME,SIZE,MODEL,SERIAL     # which serial is gone

# 2. Every /hddRaid1 fstab line already carries `degraded` and `nofail`, so it
#    mounts with one drive missing. To mount it by hand:
sudo mount -o degraded,noatime,nossd,space_cache=v2 \
  UUID=8b66e847-4273-4e2a-ad53-b312b3b3ee6d /hddRaid1

# 3. Install replacement 24TB drive -- address it by serial, never by letter
NEW=/dev/disk/by-id/ata-<model>_<new-serial>
sudo wipefs -a "$NEW"

# 4. Replace (this will take DAYS for 24TB drives), in the foreground (-B):
#    it returns when the replace is done, and only then may the balance start
sudo btrfs replace start -B <missing-devid> "$NEW" /hddRaid1
# Monitor from another terminal: sudo btrfs replace status -1 /hddRaid1

# 5. Convert anything written while degraded back to RAID-1
sudo btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /hddRaid1

# Expect 24-72 hours depending on data volume (~8.3 TiB to sync, 2026-10-02)
```

**Replacement must be**: >= 21.83 TiB (24 TB, such as the ST24000DM001, or equivalent)

---

## 8. Post-Replacement Verification

Run these checks after ANY drive replacement:

### 8a. BTRFS Integrity

```bash
# Full scrub (reads every block on both drives, verifies checksums)
sudo btrfs scrub start -B /          # NVMe -- ~15-30 min
sudo btrfs scrub start -B /opt       # SSD -- ~5-10 min (data is RAID0: errors are detected, not repaired)
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
# Correct profiles: RAID1 everywhere except the SSD pool's data (RAID0)
```

### 8d. ESP Sync (NVMe replacement only)

```bash
# Verify both ESPs have identical content
sudo bash -c 'diff <(cd /boot && find . -type f -exec md5sum {} + | sort -k2) \
                   <(cd /mnt/esp-backup && find . -type f -exec md5sum {} + | sort -k2)'
# Only the files is_unique_file() exempts may differ: loader/random-seed (per-ESP
# entropy), loader/.#bootctl* (bootctl temp files) and test-sync-trigger

# If different, resync:
sudo /usr/local/bin/esp-sync.sh
```

### 8e. Snapper Configuration

```bash
# Verify all snapper configs are intact
sudo snapper list-configs

# Configs present on 2026-10-02 (19; config files are root-only, so which
# subvolume each covers was not re-read):
# root, home, root-home, var-lib-audiobooks, etc-audiobooks,
# opt, srv, hibp-data,
# claude-code, claude-config, Audiobooks, steam-library, audiobook-manager,
# claude-test-skill, hibp-project, mcp-workspace, repo-templates, steam-sam,
# streaming-feature
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
| NVMe BTRFS | `20b5fa7e-d8c0-4035-ae45-f80263073a96` | partition 2 of NVMe `204445805771` + `20465F802394` |
| SSD BTRFS (`sata_pool`) | `2638d087-0be1-436e-bfe4-8d6551ec02be` | 860 PRO + 850 EVO mSATA (letters drift) |
| HDD BTRFS | `8b66e847-4273-4e2a-ad53-b312b3b3ee6d` | ST24000DM001 `ZXA0V0EY` + `ZXA0MHSK` (letters drift) |
| dasRaid0 BTRFS | `d29fdda7-a1e5-4640-996e-2b78569cb65d` | 4 × 2TB, internal SATA ([bay map](author-bay-mapping.md)) |
| DAS primary backup | `b2dbe07d-40b9-422e-8ccf-ef4931c40457` | `das-backup-22tb`, 22TB `ZXA1R71M` + `ZXA1NYGZ` |
| DAS recovery A | `60b05268-7f8f-47b5-a38a-752576a1172a` | `das-backup-system-recovery-A`, 2TB `ZK208Q77` p2 |
| DAS recovery B | `7c7ae72d-09d6-4086-b249-1ac60f21b73b` | `das-backup-system-recovery-B`, 2TB `ZFL41DNY` p2 |
| ESP recovery bay 1 | `6D15-0632` | 2TB `ZK208Q77`, `LABEL=RECOV-ESP-1` |
| ESP recovery bay 4 | `6CAB-B04D` | 2TB `ZFL41DNY`, `LABEL=RECOV-ESP-4` |
| ESP primary | `129B-4CA4` | partition 3 of NVMe `204445805771`, `LABEL=EFI` |
| ESP backup | `7DE5-027D` | partition 3 of NVMe `20465F802394`, `LABEL=EFI-BACKUP` |
| Swap (unused) | `ddba4cee-f2b9-4820-96bf-46ac82c6e779` | partition 1 of NVMe `204445805771` |
| Swap (unused) | `1966b9f0-0828-4d99-9cb8-e5138032f67b` | partition 1 of NVMe `20465F802394` |
| zram swap | regenerated at every boot | zram0 |

### All Serials at a Glance

| Serial | Model | Role | Kernel name on 2026-10-02 |
|--------|-------|------|---------------------------|
| `204445805771` | WD Black SN850X 1TB | NVMe boot drive | nvme1n1 |
| `20465F802394` | WD Black SN850X 1TB | NVMe mirror drive | nvme0n1 |
| `S5HVNA0N303556E` | Samsung 860 PRO 1TB | SSD pool | sde |
| `S246NWAG500270V` | Samsung 850 EVO mSATA 1TB | SSD pool | sdg |
| `ZXA0MHSK` | Seagate ST24000DM001 (24 TB) | HDD RAID-1 | sdh |
| `ZXA0V0EY` | Seagate ST24000DM001 (24 TB) | HDD RAID-1 | sdb |

Names drift; the serial does not. The DAS and `dasRaid0` drives are listed in [author-bay-mapping.md](author-bay-mapping.md).

### Essential Commands Cheat Sheet

```bash
# --- HEALTH CHECK ---
sudo btrfs device stats /              # NVMe errors
sudo btrfs device stats /opt           # SSD errors
sudo btrfs device stats /hddRaid1     # HDD errors
sudo btrfs filesystem show             # All arrays, device status
sudo smartctl -a /dev/disk/by-id/nvme-WDS100T1X0E-00AFY0_204445805771  # NVMe SMART (by-id)
sudo smartctl -a /dev/disk/by-id/ata-ST24000DM001-3Y7103_ZXA0V0EY  # HDD SMART (by-id, not sdX)

# --- DEGRADED OPERATIONS ---
sudo btrfs filesystem show             # Find "missing" device
mount -o degraded ...                  # Mount with one drive missing

# --- REPLACEMENT ---
sudo wipefs -a /dev/disk/by-id/<new>             # Clean new drive (by serial)
sudo btrfs replace start <devid> /dev/disk/by-id/<new> /mp   # Start replacement
sudo btrfs replace status /mountpoint            # Check progress

# --- PARTITION CLONING (NVMe only) ---
sudo sfdisk -d "$SURV" | sed '/^label-id/d; s/, *uuid=[^,]*//' | sudo sfdisk "$NEW"  # Clone layout, fresh GUIDs (§5a Step 4)
sudo mkswap "${NEW}p1"                                    # Create swap (unused)
sudo mkfs.vfat -F32 -n <EFI|EFI-BACKUP> "${NEW}p3"        # Create ESP -- label matters

# --- ESP MANAGEMENT ---
sudo /usr/local/bin/esp-sync.sh        # Manual ESP sync
efibootmgr -v                          # View boot entries
sudo efibootmgr --create --disk "$NEW" --part 3 \
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
- SSD (`S5HVNA0N303556E`): Samsung 860 PRO 1TB
- SSD (`S246NWAG500270V`): Samsung 850 EVO mSATA 1TB
- HDD: Seagate ST24000DM001-3Y7103 (24 TB)

---

## 10. Offline Backup Plan

A comprehensive offline backup strategy is documented separately in [`OFFLINE-BACKUP-PLAN.md`](../OFFLINE-BACKUP-PLAN.md).

**Summary**:
- **Hardware**: TerraMaster D6-320 (6-bay USB 3.2 Gen2 JBOD) — 4 of 6 bays occupied (bays 3 and 6 empty). *Gen2 is the rating, not a promise*: the tree ran at **480 Mbit/s** for nine days in August 2026 with no symptom other than slower backups. Verify with `cat /sys/bus/usb/devices/*/speed`, expect **10000** (Mbit/s)
- **Primary Backup (BTRFS RAID-1)**: 2x 22TB Exos (ST22000NM000C-3WC103) in bays 2 (`ZXA1R71M`, RMA replacement for failed `ZXA0LMAE` since 2026-05-15) and 5 (`ZXA1NYGZ`), single BTRFS filesystem `das-backup-22tb` UUID `b2dbe07d-40b9-422e-8ccf-ef4931c40457`. Mounted with `degraded` so single-leg failure does not interrupt backups, restores, or recovery.
- **Recovery Drives**: 2x 2TB Barracuda (independent, NOT a RAID pair) in bays 1 (`ZK208Q77`, `das-backup-system-recovery-A`) and 4 (`ZFL41DNY`, `das-backup-system-recovery-B`) — each boots its own independent OS from its own ESP, not this system
- **Internal SATA**: dasRaid0 (4x 2TB Barracuda RAID0, general storage) — moved from DAS 2026-04-06
- **Offline spares**: 1x 2TB Barracuda (ZFL416F6, cold spare for dasRaid0)
- **Software**: btrbk 0.32.7 + mbuffer (installed; btrbk version as of 2026-10-02), orchestrated by DAS-Backup-Manager 0.7.23.0
- **Irreplaceable data**: ~1 TiB (NVMe subvolumes, SSD /opt + /srv, ClaudeCodeProjects, audiobook sources)
- **Backed up**: every subvolume on the NVMe, SSD, HDD and dasRaid0 filesystems has a `config.toml` entry, and a backup run adopts any new one automatically (to `<first-source>-adopted/` on the primary when it has no configured parent). VMs, `SteamLibrary` and `bosco-media` (source `hdd-media`), `@steam` and the `Audiobooks` library go to the primary only; `ISOs`, `SteamLibrary-local` and `claude-config` (source `hdd-system`), system, SSD (source `ssd`) and project data also go to both recovery drives — `ISOs` and `SteamLibrary-local` are an accepted exception to the bulk-data-primary-only rule (2026-10-02): small (9 GiB and 5 GiB) and static, not worth the series churn of moving them
- **Not backed up**: `@cache` (and `@cache/stremio`), `@tmp`, `@var-tmp`, `coredumps`, `@/var/lib/machines`, `@/var/lib/portables` (excluded), and every `.snapshots` / `.btrbk-snapshots` tree
- **Status**: Active — primary backup runs with live RAID-1 redundancy (added 2026-05-06)

---

*Document generated: 2026-02-01, updated 2026-05-06 (added second 22TB CMR drive in bay 5, das-backup-22tb converted to BTRFS RAID-1), 2026-10-02 (re-keyed by serial after the board swap; SSD pool profile, subvolumes, swap, boot entries), 2026-10-07 (boot entries read from the live files, 24 project subvolumes, replace and ESP-sync corrections). UUIDs, serials, PARTUUIDs, profiles and partition layouts verified against the running system on 2026-10-02, the systemd-boot entry files on 2026-10-07.*
