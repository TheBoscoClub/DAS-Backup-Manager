# Disaster Recovery Guide

**For system recovery from DAS backup drives**

> **Important**: Replace all `<placeholder>` values (device paths, UUIDs, serials, bay references) with your actual values. Run `btrdasd config show` to display your configured targets and serials.

This guide is written for users with minimal technical experience. Follow each step exactly as written, substituting your own device paths and UUIDs where indicated.

---

## Table of Contents

1. [Understanding Your Backup System](#understanding-your-backup-system)
2. [When to Use This Guide](#when-to-use-this-guide)
3. [Booting into Rescue Mode](#booting-into-rescue-mode)
4. [Recovery Scenarios](#recovery-scenarios)
   - [Scenario A: Single NVMe Drive Failure](#scenario-a-single-nvme-drive-failure)
   - [Scenario B: Both NVMe Drives Failed](#scenario-b-both-nvme-drives-failed)
   - [Scenario C: Complete System Replacement](#scenario-c-complete-system-replacement)
   - [Scenario D: 22TB RAID-1 Backup Array Single-Leg Failure](#scenario-d-22tb-raid-1-backup-array-single-leg-failure)
5. [Step-by-Step Recovery Procedures](#step-by-step-recovery-procedures)
6. [Common Boot Repairs](#common-boot-repairs)
   - [Reset a Forgotten Root Password](#reset-a-forgotten-root-password)
   - [Fix a Broken /etc/fstab](#fix-a-broken-etcfstab)
   - [Fix a Broken Bootloader or Missing Kernel](#fix-a-broken-bootloader-or-missing-kernel)
   - [Fix a Systemd Service That Hangs Boot](#fix-a-systemd-service-that-hangs-boot)
   - [Fix a Read-Only Root Filesystem](#fix-a-read-only-root-filesystem)
7. [Restoring Individual Files and Subvolumes](#restoring-individual-files-and-subvolumes)
   - [Browse Backup Snapshots](#browse-backup-snapshots)
   - [Restore a Single File](#restore-a-single-file)
   - [Restore an Entire Subvolume](#restore-an-entire-subvolume)
8. [Keeping the recovery OSes current](#keeping-the-recovery-oses-current)
9. [Troubleshooting](#troubleshooting)
10. [Reference Information](#reference-information)

---

## Understanding Your Backup System

### Hardware

- **DAS enclosure**: Your external storage enclosure (any manufacturer, any interface -- USB, Thunderbolt, eSATA)
- **Backup drives**: BTRFS-formatted drives with btrbk snapshot history

### Drive Layout

Your DAS bay mapping (see [DAS-BAY-MAPPING.md](DAS-BAY-MAPPING.md)) documents which bay holds which drive. A typical configuration might include:

- **Bootable recovery drive(s)**: Drives with an ESP + their own, independent OS installation (role `mirror` in `config.toml`). They are **not** a copy of your system and nothing writes your system's ESP onto them
- **Primary backup drive**: Large-capacity drive (or BTRFS RAID-1 pair) receiving all btrbk snapshots (role `primary`)
- **General storage**: Optional expendable-data drives

### What's Backed Up

Your backup targets are defined in `/etc/das-backup/config.toml`. Common categories:

- **System backup**: OS, applications, home folder, system configuration
- **Data backup**: Projects, documents, media source files
- **Recovery drives**: An independent bootable OS on each drive; they also receive the btrbk snapshots of every source whose `target_labels` is empty (`[]`), with their own (usually short) retention

### Where Each Backup Lives on a Target

Every snapshot sits at `<target mount>/<source target_subdir>/<snapshot_name>.<YYYYMMDDTHHMM>`, where `target_subdir` and `snapshot_name` come from `/etc/btrbk/btrbk.conf` (generated from `config.toml`; `btrdasd subvol list` shows the entries). On the author's system, for example, `@` is `/mnt/backup-22tb/nvme/root-.20261002T1500` and `@home` is `/mnt/backup-22tb/nvme/home.20261002T1500`. Two snapshots in the same minute get a `_1`, `_2`, … suffix.

- **Subvolumes adopted automatically**: every backup run adopts new subvolumes. One nested inside a configured subvolume joins that entry's source (same directory, same targets). Any other lands in a source named `<first source on that volume>-adopted`, sent to the **primary target only** — look under `<primary mount>/<first-source>-adopted/` (e.g. `/mnt/backup-22tb/ssd-adopted/`).
- **Retired subvolumes**: when a subvolume disappears its entry is marked `retired` and no new snapshots are taken, but its last snapshots stay on each target and are restorable until that target's longest retention window has passed since the retirement date; then the run deletes them.
- **Boot copies (primary target only)**: the top level of the primary target holds writable `@` and `@home` made from the latest `root-`/`home` snapshots (rebuilt on full runs), plus `@.archive.<TS>` / `@home.archive.<TS>` of the ones they replaced (kept 60 days by default). On a recovery drive, `@` and `@home` are that drive's **own** OS — never restore your system from them.

### Backup Schedule

As configured by `btrdasd setup`:
- **Nightly**: Incremental backup (only changed files since last snapshot)
- **Configurable**: Full backup refresh on a schedule you define

---

## When to Use This Guide

Use this guide when:

1. Your computer will not boot normally
2. You see disk errors on startup
3. Your system reports "drive not found"
4. You need to restore files from backup
5. You are setting up a new or replacement computer

**Important**: If only one drive in a RAID-1 array fails, your system may still boot normally due to mirroring. This guide covers that scenario too.

---

## Booting into Rescue Mode

### Prerequisites

- Your DAS must have at least one bootable recovery drive (with ESP + OS)
- If you have no bootable recovery drives, skip to [Scenario B](#scenario-b-both-nvme-drives-failed) and use a Linux live USB instead

### Step 1: Connect the DAS

1. Plug your DAS enclosure into any available USB (or Thunderbolt/eSATA) port
2. Turn on the DAS using its power switch
3. Wait for all drive LEDs to indicate ready state (typically 15-30 seconds)

### Step 2: Enter Boot Menu

1. Restart your computer
2. **Immediately** press the boot menu key repeatedly:
   - ASUS motherboards: **F8**
   - Gigabyte: **F12**
   - MSI: **F11**
   - Most other PCs: **F12**, **F11**, or **F8**

3. If you miss it, restart and try again

### Step 3: Select DAS Boot Entry

In the boot menu, look for entries corresponding to your DAS drives. They will typically show the DAS enclosure model name followed by a partition UUID. For example:

```
<DAS-model> (<your-esp-uuid>)     <-- Primary bootable recovery drive
<DAS-model> (<your-esp-uuid>)     <-- Mirror bootable recovery drive (if configured)
```

Select either one and press **Enter**.

Firmware boot entries are numbered by the firmware and renumbered whenever its NVRAM is reset (a motherboard swap does this), and auto-created entries are often all named `UEFI OS`. Never record or follow a recovery step by entry number: identify each recovery drive's ESP by its PARTUUID or label (`efibootmgr -v` on a running system prints the PARTUUID of every entry; `lsblk -o NAME,SERIAL,LABEL,PARTUUID` maps it to a drive).

### Step 4: Choose Rescue Environment

Your bootloader menu will appear with options configured during setup. Select the rescue or recovery entry.

If you set up a graphical rescue environment (e.g., XFCE), you will get a desktop with recovery tools. Otherwise, you will boot to a command line.

### Step 5: Login

Use the credentials you configured for the recovery environment.

---

## Recovery Scenarios

### Scenario A: Single NVMe Drive Failure

**Symptoms**: System still boots but shows "degraded array" warnings.

**What to do**:
1. Boot into your normal system (it should still work on the surviving mirror)
2. Open a terminal and check array status:
   ```bash
   sudo btrfs device stats /
   ```
3. If errors show on one device, replace that drive
4. See [Replacing a Failed Boot Drive](#replacing-a-failed-boot-drive)

---

### Scenario B: Both NVMe Drives Failed

**Symptoms**: Computer will not boot at all, or BIOS shows "No bootable device".

**What to do**:
1. Boot into Rescue Mode (see [Booting into Rescue Mode](#booting-into-rescue-mode)), or boot from a Linux live USB
2. You can either:
   - **Option 1**: Boot a recovery drive's own OS from the DAS and work from there (temporary, slow over USB)
   - **Option 2**: Restore backup to new internal drives (permanent fix)

See [Full System Restoration](#full-system-restoration) for detailed steps.

---

### Scenario C: Complete System Replacement

**Symptoms**: You have new hardware (new motherboard, CPU, etc.) and need to restore your system.

**What to do**:
1. Install new drives in the new system
2. Connect the DAS
3. Boot into Rescue Mode (or a Linux live USB)
4. Restore backup to new drives
5. Update hardware-specific drivers if needed

See [Restoring to New Hardware](#restoring-to-new-hardware) for detailed steps.

---

### Scenario D: 22TB RAID-1 Backup Array Single-Leg Failure

**Applies if** your primary backup is a BTRFS RAID-1 across two large drives (in this setup: 22TB Exos drives in DAS bays 2 and 5, sharing BTRFS UUID `b2dbe07d-40b9-422e-8ccf-ef4931c40457`).

**Symptoms**:
- The backup log (`journalctl -u das-backup`) says `primary-22tb: present=[…] missing=[…] — RAID-1 degraded, proceeding`. As of `backup-run.sh` v4.7.1 the emailed report has no line of its own for this and its SMART section shows one serial per target, so a clean-looking email does not prove both legs are present
- `sudo btrfs filesystem show /mnt/backup-22tb` says `*** Some devices missing`
- `sudo btrfs device stats /mnt/backup-22tb` shows non-zero error counters on one leg

**Why this is a separate scenario**: This array is not in `/etc/fstab` and has nothing to do with system boot. The system continues booting and running normally on its NVMe RAID-1. What needs recovery is the *backup target itself* — so that incremental backups, restores, and disaster-recovery procedures keep working during the days it takes to replace a 22TB drive.

#### Why backups still work in degraded mode

`/etc/das-backup/config.toml` sets `[das].mount_opts` to include `degraded`. The `backup-run.sh` script mounts the target with these options, so a missing leg does not abort the nightly backup. The downside: **any data written while degraded is allocated as `single` profile** (not redundant). After the failed leg is replaced, a balance restores RAID-1 across all chunks. Until then, only one copy of recent data exists.

#### Step 1: Confirm which leg failed

```bash
sudo btrfs filesystem show /mnt/backup-22tb
# Output looks like:
#   Label: 'das-backup-22tb' uuid: b2dbe07d-40b9-422e-8ccf-ef4931c40457
#       Total devices 2 FS bytes used X.XTiB
#       devid    1 size 20.01TiB used Y path /dev/sdX1
#       devid    2 size 0 used 0 path MISSING
# (the "MISSING" line — note that devid number)

sudo btrfs device stats /mnt/backup-22tb
# Look for non-zero counters: write_io_errs, read_io_errs, corruption_errs
```

Cross-reference the device serial against your bay map (`docs/examples/author-bay-mapping.md`):
- `ZXA1R71M` (bay 2, devid 2) — RMA replacement for failed `ZXA0LMAE` since 2026-05-15. Note: devid numbering was reversed by the 2026-05-07 `mkfs.btrfs` rebuild — the surviving leg became devid 1.
- `ZXA1NYGZ` (bay 5, devid 1) — was devid 2 prior to 2026-05-07

#### Step 2: Mount the array degraded if it failed to mount

The `backup-run.sh` script always mounts by UUID with `[das].mount_opts` (which include `degraded`), so scheduled backups continue. For interactive use, mount the same way:

```bash
# Is a backup, scrub, restore, index, reconcile or doctor run holding the DAS?
# They mount and unmount this target themselves, and all hold this lock while
# they do; `cat /run/das-maintenance.lock` shows what the holder recorded.
sudo flock -n /run/das-maintenance.lock true || echo "WAIT"

# If /mnt/backup-22tb is not currently mounted
sudo mkdir -p /mnt/backup-22tb
sudo mount -t btrfs -o noatime,compress=zstd:3,space_cache=v2,autodefrag,commit=120,nossd,degraded \
    UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup-22tb
```

A backup that starts while you have it mounted uses your mount and unmounts it when it finishes; unmount it yourself when you are done.

The backup targets are hidden from udisks by the generated rule `/etc/udev/rules.d/99-das-backup-udisks-ignore.rules`, so no desktop session mounts them and nothing should appear under `/run/media`. If one does, the rule is not applying: unmount it and run `sudo btrdasd setup --check`.

#### Step 3: Verify SMART on the surviving leg

Before relying on the surviving drive for days while the replacement is sourced and rebuilt:

```bash
# Address the drive by serial, never by /dev/sdX letter (letters change on every reconnect)
SURV=/dev/disk/by-id/ata-ST22000NM000C-3WC103_<surviving-serial>
sudo smartctl -a -d sat "$SURV"          # Quick attribute view
sudo smartctl -t short -d sat "$SURV"    # 2-min sanity test
# Optional: full extended test (~38h, runs in firmware, no host I/O hit)
sudo smartctl -t long -d sat "$SURV"
```

If the surviving drive shows reallocated sectors or pending sectors, copy the most critical recent snapshots elsewhere immediately — running degraded on a marginal drive is a one-failure-from-data-loss situation.

#### Step 4: Source a replacement drive

- **Required**: equal or larger capacity (≥ 20.01 TiB usable).
- **Recommended**: same model (Seagate ST22000NM000C-3WC103) for matching speed and behavior. Different model is acceptable.
- **Risk hedge**: prefer a drive from a different manufacturing batch than the surviving leg to avoid correlated failure.

When the new drive arrives, run a full SMART extended test (~38 hours) before committing data:

```bash
NEW=/dev/disk/by-id/ata-<model>_<new-serial>   # from: ls /dev/disk/by-id/ | grep -v part
sudo smartctl -i -d sat "$NEW"           # Confirm capacity matches
sudo smartctl -t short -d sat "$NEW"     # 2-min DOA check
sudo smartctl -t long  -d sat "$NEW"     # 38h extended test
# Wait for completion, then:
sudo smartctl -l selftest -d sat "$NEW"
# All tests should show "Completed without error"
```

You can begin Step 5 in parallel with the long test — the test runs in the drive's firmware in offline mode and yields to host I/O.

#### Step 5: Power off DAS, swap drive in, power up

Use your bay map to identify the failed drive's bay before pulling. The DAS does not require host shutdown — only DAS power-cycling.

#### Step 6: Partition the new drive identically

The replacement must have a GPT partition that exactly matches the surviving leg's geometry. Address it by its serial (`$NEW` from Step 4) — `--zap-all` on the wrong drive destroys it, and `/dev/sdX` letters move on every reconnect. Confirm with `lsblk -o NAME,SIZE,SERIAL,TRAN` that the serial is the new drive's and that it has no partitions:

```bash
sudo sgdisk --zap-all "$NEW"
sudo sgdisk --new=1:2048:42970644446 --typecode=1:8300 \
    --change-name=1:das-backup-22tb "$NEW"
sudo partprobe "$NEW"
```

Verify with `sudo sgdisk --print "$NEW"` — the partition should be sectors 2048–42970644446, 20.0 TiB, type 8300, name `das-backup-22tb`. Its partition is `"$NEW"-part1`.

#### Step 7: Replace the failed device in the array

```bash
# Get the missing devid from `btrfs filesystem show` (Step 1)
MISSING_DEVID=<number from "MISSING" line>

# Start the replace — runs in background by default
sudo btrfs replace start "$MISSING_DEVID" "$NEW"-part1 /mnt/backup-22tb

# Monitor (recover takes ~24-48 hours for ~5 TiB over USB)
watch -n 60 sudo btrfs replace status /mnt/backup-22tb
```

`btrfs replace` reads from the surviving leg, writes to the new device, and updates the superblock. It is online — backups can continue running concurrently (slower).

#### Step 8: Scrub, then restore RAID-1 across single-profile chunks

First scrub, so every block is verified (and repaired from its good copy) before anything is rewritten:

```bash
# Full read of every block on both legs, repairs any checksum mismatches
sudo btrfs scrub start -B /mnt/backup-22tb
sudo btrfs scrub status /mnt/backup-22tb
# "Error summary: no errors found" is what you want
```

Then convert: any data that was written while the array was degraded is in `single` profile chunks.

```bash
# `soft` filter only touches chunks that aren't already RAID-1
sudo btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft \
    /mnt/backup-22tb

# Watch progress
sudo btrfs balance status /mnt/backup-22tb
```

#### Step 9: Verify and reset counters

```bash
# Confirm all error counters are zero
sudo btrfs device stats /mnt/backup-22tb

# Reset stats to baseline now that the array is healthy
sudo btrfs device stats --reset /mnt/backup-22tb

# Confirm RAID-1 across the board
sudo btrfs filesystem df /mnt/backup-22tb
# Expect: Data, RAID1 / Metadata, RAID1 / System, RAID1 (no `single` lines)
```

Unmount it when you are done (`sudo umount /mnt/backup-22tb`); the next backup mounts it itself.

#### Step 10: Update the config, bay map and CHANGELOG

Replace the failed serial with the new one in the target's `serials` in `/etc/das-backup/config.toml`, then run `sudo btrdasd setup --upgrade` (it regenerates the udisks-ignore rule from the serials) and `sudo btrdasd setup --check`. The filesystem UUID, and so `mount_uuid`, does not change with a `btrfs replace`.

Update `docs/examples/author-bay-mapping.md` with the new drive's serial, PARTUUID, and BTRFS UUID_SUB (from `sudo blkid "$NEW"-part1`). Update CHANGELOG.md to record the replacement date and the failure cause.

---

## Step-by-Step Recovery Procedures

### Replacing a Failed Boot Drive

**You will need**: New drive (same or larger capacity than the failed one)

**Time required**: About 1-2 hours

1. **Shut down the computer** completely

2. **Replace the failed drive**:
   - Open your computer case
   - Remove the failed drive (note which slot it was in)
   - Install the new drive in the same slot

3. **Boot from the surviving drive** (or from the DAS rescue environment)

4. **Open a terminal**

5. **Identify the new drive**:
   ```bash
   lsblk -d -o NAME,SIZE,MODEL,SERIAL
   ```
   The new drive will show with no partitions. Pick it by **serial**: NVMe names (`nvme0n1`, `nvme1n1`) can swap between boots just like `/dev/sdX` letters.

6. **Partition the new drive** (replace `<new-drive>` with the device you just matched by serial):
   ```bash
   # Clone partition table from surviving drive
   # sfdisk -d carries the disk GUID (label-id:) and every partition's uuid=, and
   # sfdisk applies them: drop both so the new drive gets fresh PARTUUIDs instead of
   # duplicating the survivor's (PARTUUIDs identify the ESPs and their firmware entries)
   sudo sfdisk -d /dev/<surviving-drive> | sed '/^label-id/d; s/, *uuid=[^,]*//' | sudo sfdisk /dev/<new-drive>

   # Or create manually:
   sudo parted /dev/<new-drive> mklabel gpt
   sudo parted /dev/<new-drive> mkpart ESP fat32 1MiB 4GiB
   sudo parted /dev/<new-drive> set 1 esp on
   sudo parted /dev/<new-drive> mkpart primary 4GiB 100%

   # Format the ESP with the label your fstab and ESP-mirroring expect
   # (author's system: EFI on the boot drive, EFI-BACKUP on the mirror)
   sudo mkfs.fat -F32 -n <esp-label> /dev/<new-drive-esp-partition>
   ```

7. **Replace the missing device in the BTRFS array**:
   ```bash
   # Mount the surviving drive degraded (if not already mounted)
   sudo mount -o degraded /dev/<surviving-btrfs-partition> /mnt

   # Note the devid of the missing device
   sudo btrfs filesystem show /mnt

   # Rebuild the missing copy onto the new partition
   sudo btrfs replace start <missing-devid> /dev/<new-drive-btrfs-partition> /mnt

   # Anything written while degraded is in `single` chunks: convert back
   sudo btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt
   ```

8. **Wait for the replace and balance to complete** (can take several hours):
   ```bash
   sudo btrfs replace status /mnt
   sudo btrfs balance status /mnt
   ```

9. **Populate the new ESP** — never by hand-copying files from one ESP to another:
   - If the new drive is the **mirror**, run your ESP-mirroring mechanism (author's system: `sudo /usr/local/bin/esp-sync.sh`, which only copies `/boot` → `/mnt/esp-backup`, removes from the mirror any file `/boot` lacks, and refuses any non-NVMe device).
   - If it held the **primary** ESP (mounted at `/boot`), mount it there and reinstall instead. **`/boot` must be complete before the first sync**, because the sync deletes from the mirror — the ESP you just booted from — whatever `/boot` lacks, and on the author's system a pacman hook (`esp-mirror.hook`) runs that sync at the end of any kernel transaction. Put back first anything no package or generator writes (hand-written loader entries), then reinstall in ONE transaction every package that owns a file on the ESP — `amd-ucode` owns `/boot/amd-ucode.img`, not the kernel package — plus every installed kernel:
     ```bash
     sudo mount /boot                      # LABEL=EFI from fstab — now the new ESP
     sudo bootctl install
     # recreate hand-written loader entries in /boot/loader/entries/ now
     # list the ESP-owning packages: pacman -Ql | grep ' /boot/' | awk '{print $1}' | sort -u
     sudo pacman -S amd-ucode linux-cachyos   # add linux-cachyos-lts etc. if installed
     sudo sdboot-manage gen
     sudo /usr/local/bin/esp-sync.sh --dry-run   # must print no "would remove" line
     sudo /usr/local/bin/esp-sync.sh
     ```
     The author's full sequence, including a preview taken before the transaction, is `examples/author-storage-reference.md` §5a Step 8.
   - Never point any copy or sync at a DAS recovery drive's ESP: those boot their own independent OS.

10. **Register UEFI boot entry for the new drive**:
    ```bash
    sudo efibootmgr --create --disk /dev/<new-drive> --part <esp-partition-number> \
      --loader '\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI' \
      --label "<your-boot-label>" --unicode
    ```

11. **Update fstab** with new UUIDs if needed (an fstab that mounts the ESPs by `LABEL=`, as the author's does, needs no change when the new ESP got the same label):
    ```bash
    sudo blkid /dev/<new-drive-esp-partition>   # Get new ESP UUID
    sudo vim /etc/fstab                          # Update UUIDs
    ```

12. **Reboot** and test

---

### Full System Restoration

**You will need**: Two new drives for RAID-1 (or one drive for single-device setup), plus access to DAS backup

**Time required**: About 2-4 hours depending on data size

1. **Boot into Rescue Mode** (from DAS or Linux live USB)

2. **Partition new drives** (replace device names with your actual devices):
   ```bash
   # For each drive:
   sudo parted /dev/<drive> mklabel gpt
   sudo parted /dev/<drive> mkpart ESP fat32 1MiB 4GiB
   sudo parted /dev/<drive> set 1 esp on
   sudo parted /dev/<drive> mkpart primary 4GiB 100%
   sudo mkfs.fat -F32 /dev/<drive-esp-partition>
   ```

3. **Create BTRFS filesystem** on the main partitions:
   ```bash
   # RAID-1 with two drives:
   sudo mkfs.btrfs -m raid1 -d raid1 /dev/<drive1-btrfs-partition> /dev/<drive2-btrfs-partition>

   # Or single drive:
   sudo mkfs.btrfs /dev/<drive-btrfs-partition>
   ```

4. **Mount the new filesystem**:
   ```bash
   sudo mkdir -p /mnt/target
   sudo mount /dev/<drive1-btrfs-partition> /mnt/target
   ```

5. **Mount the DAS backup** — the **primary** target, by filesystem UUID. Do not use a recovery drive's `@`/`@home`: those are that drive's own OS, not your system:
   ```bash
   # Find the target's UUID (author's primary: das-backup-22tb, b2dbe07d-40b9-422e-8ccf-ef4931c40457)
   lsblk -o NAME,SERIAL,LABEL,UUID

   # Mount its top level read-only (degraded is harmless with both RAID-1 legs present)
   sudo mkdir -p /mnt/backup
   sudo mount -t btrfs -o ro,degraded UUID=<primary-target-uuid> /mnt/backup

   # Pick the snapshot to restore from (newest last)
   ls /mnt/backup/nvme/ | grep -E '^(root-|home)\.'
   ```

6. **Restore the system**:
   ```bash
   # Create subvolumes matching your original layout
   sudo btrfs subvolume create /mnt/target/@
   sudo btrfs subvolume create /mnt/target/@home
   sudo btrfs subvolume create /mnt/target/@log
   sudo btrfs subvolume create /mnt/target/@root
   # Add any other subvolumes from your configuration (`btrdasd subvol list`);
   # a nested one (e.g. @/@audiobooks-db) is created inside its parent after the parent is restored

   # Copy root data from the chosen snapshot
   sudo rsync -aAXHv --info=progress2 /mnt/backup/nvme/root-.<TIMESTAMP>/ /mnt/target/@/

   # Restore home from the snapshot of the same time
   sudo rsync -aAXHv --info=progress2 /mnt/backup/nvme/home.<TIMESTAMP>/ /mnt/target/@home/
   ```
   The subvolume names above are the author's (`btrdasd subvol list` shows yours); a snapshot holds an empty directory where a nested subvolume sat, so restore each nested one from its own snapshot.

7. **Install bootloader**:
   ```bash
   # Mount ESP
   sudo mount /dev/<drive-esp-partition> /mnt/target/@/boot

   # Chroot and install bootloader
   sudo arch-chroot /mnt/target/@    # Arch/CachyOS
   # Or for Debian/Ubuntu: sudo chroot /mnt/target/@

   bootctl install                    # For systemd-boot
   # Or: grub-install /dev/<drive>    # For GRUB
   exit
   ```

8. **Update fstab with new UUIDs**:
   ```bash
   # Get new UUIDs
   sudo blkid /dev/<drive-esp-partition>
   sudo blkid /dev/<drive-btrfs-partition>

   # Edit fstab in the restored system
   sudo nano /mnt/target/@/etc/fstab
   # Replace old UUIDs with new ones
   ```

9. **Unmount and reboot**:
   ```bash
   sudo umount -R /mnt/target
   sudo umount /mnt/backup
   sudo reboot
   ```

---

### Restoring to New Hardware

Follow the [Full System Restoration](#full-system-restoration) procedure, then:

1. After first boot, update all packages and regenerate initramfs:
   ```bash
   # Arch/CachyOS:
   sudo pacman -Syu
   sudo mkinitcpio -P

   # Debian/Ubuntu:
   sudo apt update && sudo apt upgrade
   sudo update-initramfs -u

   # Fedora:
   sudo dnf upgrade
   sudo dracut --force
   ```

2. If using different GPU than original, install appropriate drivers:
   ```bash
   # AMD GPU (Arch example)
   sudo pacman -S mesa vulkan-radeon

   # NVIDIA GPU
   sudo pacman -S nvidia nvidia-utils

   # Intel GPU
   sudo pacman -S mesa vulkan-intel
   ```

3. Regenerate initramfs:
   ```bash
   sudo mkinitcpio -P    # Arch/CachyOS
   # Or appropriate command for your distro
   ```

4. Reboot

---

## Common Boot Repairs

These procedures fix the most common reasons a Linux system won't boot. In every case, you boot from this recovery drive first, then fix the broken system from the outside.

### Preparation: Mount the Broken System

Before any repair below, you need to mount the broken system's root filesystem. These steps are the same for all repairs:

```bash
# 1. Find the broken system's drive
lsblk -f
# Look for the BTRFS partition with your system's UUID or label

# 2. Mount it
sudo mkdir -p /mnt/broken
sudo mount -o subvol=@ /dev/<broken-system-partition> /mnt/broken

# 3. If you also need to fix boot files, mount the ESP
sudo mount /dev/<broken-system-esp> /mnt/broken/boot

# 4. For operations that need a running system (mkinitcpio, passwd, systemctl),
#    set up a chroot:
sudo mount --bind /dev  /mnt/broken/dev
sudo mount --bind /proc /mnt/broken/proc
sudo mount --bind /sys  /mnt/broken/sys
sudo mount --bind /run  /mnt/broken/run
sudo chroot /mnt/broken
```

When you are done with any repair, exit the chroot and unmount:
```bash
exit                          # leave chroot
sudo umount -R /mnt/broken    # unmount everything
sudo reboot
```

---

### Reset a Forgotten Root Password

**Symptoms**: You cannot log in as root or use `sudo`. No system damage -- you just need the password reset.

**When booted from this recovery drive:**

1. Mount the broken system and enter chroot (see [Preparation](#preparation-mount-the-broken-system) above)

2. **Reset the root password**:
   ```bash
   passwd root
   ```
   Type the new password twice when prompted. There is no output while typing -- this is normal.

3. **If you also need to reset a user password**:
   ```bash
   passwd <username>
   ```

4. **If sudo is broken** (user removed from wheel group, sudoers corrupted):
   ```bash
   # Add user back to wheel group
   usermod -aG wheel <username>

   # Or fix sudoers (this opens a safe editor that checks syntax)
   visudo
   # Make sure this line exists and is NOT commented out:
   #   %wheel ALL=(ALL:ALL) ALL
   ```

5. Exit chroot, unmount, reboot.

---

### Fix a Broken /etc/fstab

**Symptoms**: System starts booting but hangs or drops to an emergency shell with messages like:
- "A dependency job for local-fs.target failed"
- "You are in emergency mode"
- "Failed to mount /home" or any other mount point
- "Timed out waiting for device"

**When booted from this recovery drive:**

1. Mount the broken system (see [Preparation](#preparation-mount-the-broken-system) above). You do NOT need a full chroot for this repair -- just mount the root filesystem.

2. **Look at the current fstab**:
   ```bash
   cat /mnt/broken/etc/fstab
   ```

3. **Identify the problem**. Common issues:
   - **Wrong UUID**: A drive was replaced and the UUID changed
   - **Missing drive**: An entry references a drive that no longer exists
   - **Typo in mount options**: A misspelled option prevents mounting
   - **Wrong subvolume name**: BTRFS subvolume was renamed or deleted

4. **Get the correct UUIDs**:
   ```bash
   # Show all detected filesystems with UUIDs
   blkid

   # Show block devices with filesystem info
   lsblk -f
   ```

5. **Edit the fstab**:
   ```bash
   sudo nano /mnt/broken/etc/fstab
   ```

   **Key rules**:
   - Every `UUID=` must match an actual device from `blkid` output
   - If a drive is gone and you don't have a replacement, **comment out the line** by putting `#` at the start
   - The root (`/`) entry MUST be correct or the system will not boot at all
   - Check that `subvol=` names match actual BTRFS subvolumes:
     ```bash
     sudo btrfs subvolume list /mnt/broken
     ```

6. **Save the file** (in nano: Ctrl+O, Enter, Ctrl+X) and unmount:
   ```bash
   sudo umount /mnt/broken
   sudo reboot
   ```

**Tip**: If you are unsure what the fstab should look like, you can generate a fresh one:
```bash
genfstab -U /mnt/broken
```
This prints what fstab SHOULD contain based on currently mounted filesystems. Compare it to the existing file and fix discrepancies.

---

### Fix a Broken Bootloader or Missing Kernel

**Symptoms**:
- "No bootable device found"
- Bootloader menu appears but selecting an entry fails
- "vmlinuz not found" or "initramfs not found"
- Boot drops to a `systemd-boot` error or GRUB rescue shell

**When booted from this recovery drive:**

1. Mount the broken system AND its ESP, then enter chroot (see [Preparation](#preparation-mount-the-broken-system) above)

2. **Check what's on the ESP**:
   ```bash
   ls /boot/
   # You should see: vmlinuz-linux-*, initramfs-linux-*, amd-ucode.img or intel-ucode.img, EFI/, loader/
   ```

3. **If kernel files are missing**, reinstall the kernel:
   ```bash
   # CachyOS/Arch:
   pacman -S linux-cachyos    # or whichever kernel package you use
   # This reinstalls the kernel AND regenerates initramfs. If the microcode image is
   # missing too, put amd-ucode (or intel-ucode) in the SAME command: on the author's
   # system a pacman hook then syncs /boot to the backup ESP and deletes there
   # whatever /boot still lacks.

   # Debian/Ubuntu:
   apt install --reinstall linux-image-$(uname -r)
   update-initramfs -u
   ```

4. **If boot entries are missing or wrong** (systemd-boot):
   ```bash
   # List current entries
   ls /boot/loader/entries/

   # If empty or corrupt, reinstall systemd-boot
   bootctl install

   # Then create a boot entry (CachyOS example):
   cat > /boot/loader/entries/linux-cachyos.conf << 'ENTRY'
   title   CachyOS
   linux   /vmlinuz-linux-cachyos
   initrd  /amd-ucode.img
   initrd  /initramfs-linux-cachyos.img
   options root=UUID=<your-root-uuid> rw rootflags=subvol=/@
   ENTRY
   ```
   Replace `<your-root-uuid>` with your actual root partition UUID from `blkid`.

5. **If initramfs is corrupt or missing**, regenerate it:
   ```bash
   # CachyOS/Arch:
   mkinitcpio -P     # regenerates ALL initramfs images

   # Debian/Ubuntu:
   update-initramfs -u -k all
   ```

6. **Register the UEFI boot entry** (if BIOS doesn't see the drive):
   ```bash
   efibootmgr --create --disk /dev/<drive> --part <esp-number> \
     --loader '\EFI\systemd\systemd-bootx64.efi' \
     --label "CachyOS" --unicode
   ```

7. Exit chroot, unmount, reboot.

---

### Fix a Systemd Service That Hangs Boot

**Symptoms**:
- Boot freezes at "A start job is running for ..." and never finishes
- Boot hangs for 90+ seconds, then drops to a degraded shell
- System boots but critical services (networking, display manager) are broken

**When booted from this recovery drive:**

1. Mount the broken system and enter chroot (see [Preparation](#preparation-mount-the-broken-system) above)

2. **Find the failing service**. If you saw the service name on screen during the hang, use that. Otherwise:
   ```bash
   # List services that failed on last boot
   systemctl --root=/mnt/broken list-units --state=failed

   # Or from inside chroot:
   journalctl -b -1 -p err    # errors from last boot
   ```

3. **Disable the problem service** so the system can boot:
   ```bash
   # From inside chroot:
   systemctl disable <service-name>

   # Or without chroot, by removing the symlink directly:
   rm /mnt/broken/etc/systemd/system/multi-user.target.wants/<service-name>.service
   ```

4. **Common problem services and fixes**:

   | Service | Common Cause | Fix |
   |---------|-------------|-----|
   | `NetworkManager-wait-online` | Waiting for a network that doesn't exist | `systemctl disable NetworkManager-wait-online` |
   | Any `.mount` unit | Corresponds to an fstab entry | Fix fstab (see [above](#fix-a-broken-etcfstab)) |
   | `lvm2-*` or `mdadm*` | RAID/LVM array missing a device | Comment out the array in `/etc/mdadm.conf` or `/etc/lvm/lvm.conf` |
   | Display manager (sddm, gdm) | GPU driver issue | `systemctl disable sddm` then boot to TTY and fix drivers |

5. **If you need to edit a service file**:
   ```bash
   nano /mnt/broken/etc/systemd/system/<service-name>.service
   # Or find the upstream file:
   nano /mnt/broken/usr/lib/systemd/system/<service-name>.service
   ```
   To override without editing the original, create a drop-in:
   ```bash
   mkdir -p /mnt/broken/etc/systemd/system/<service-name>.service.d/
   cat > /mnt/broken/etc/systemd/system/<service-name>.service.d/override.conf << 'EOF'
   [Service]
   TimeoutStartSec=10
   EOF
   ```

6. Exit chroot, unmount, reboot. Once the system is up, investigate and fix the root cause, then re-enable the service.

---

### Fix a Read-Only Root Filesystem

**Symptoms**:
- "Read-only file system" errors when trying to save files or install packages
- System boots but nothing can be written to disk
- BTRFS errors in `dmesg` output

**When booted from this recovery drive:**

1. **First, check if the filesystem has errors**:
   ```bash
   # Find the broken system's BTRFS partition
   lsblk -f

   # Run a read-only check (safe, does not modify anything)
   sudo btrfs check --readonly /dev/<partition>
   ```

2. **If no errors found**, the filesystem may have been mounted read-only by the kernel due to a minor issue. Try:
   ```bash
   sudo mount -o remount,rw /dev/<partition> /mnt/broken
   ```
   If this works, the issue was transient. Reboot the system and see if it persists.

3. **If errors are found**, you have two options:

   **Option A — Restore from backup** (recommended, safest):
   - Follow [Restore an Entire Subvolume](#restore-an-entire-subvolume) to replace the corrupted subvolume with a known-good backup snapshot.

   **Option B — Attempt repair** (risky, last resort):
   ```bash
   # WARNING: --repair can make things worse. Only use if you have no backup
   # or the backup is also corrupted.
   sudo btrfs check --repair /dev/<partition>
   ```

4. **If the drive has SMART errors**, it may be physically failing:
   ```bash
   sudo smartctl -a /dev/<drive>
   # Look for "Reallocated_Sector_Ct" or "Current_Pending_Sector" > 0
   ```
   If the drive is failing, replace it immediately. See [Scenario A](#scenario-a-single-nvme-drive-failure) or [Scenario B](#scenario-b-both-nvme-drives-failed).

---

## Restoring Individual Files and Subvolumes

You don't always need a full system restore. Often you just need one file you accidentally deleted, or you need to roll back a subvolume to an earlier state.

### Browse Backup Snapshots

Your DAS backup drives contain dated snapshots created by btrbk. Each snapshot is a frozen copy of a subvolume at a specific point in time. The primary target keeps the long history (author's: 7 daily, 4 weekly, 12 monthly, 1 yearly); a recovery drive keeps only its own short window (author's: 7 daily). See [Where Each Backup Lives on a Target](#where-each-backup-lives-on-a-target) for the directory layout, adopted subvolumes and retired ones.

1. **Mount the backup drive** by filesystem UUID (if not already mounted). The targets are hidden from udisks, so a file manager will not offer them:
   ```bash
   sudo mkdir -p /mnt/backup
   sudo mount -t btrfs -o ro,degraded UUID=<target-uuid> /mnt/backup
   ```
   On a running system that also runs scheduled backups, check first that none is running (`sudo flock -n /run/das-maintenance.lock true || echo WAIT`), and unmount when done.

2. **List available snapshots**:
   ```bash
   # Show all subvolumes (snapshots are subvolumes)
   sudo btrfs subvolume list /mnt/backup | sort -k9

   # Example output (names in the author's layout; IDs illustrative):
   # ID 41901 gen 763 top level 5 path nvme/root-.20260930T0306
   # ID 42017 gen 766 top level 5 path nvme/root-.20261001T0312
   # ID 42188 gen 1038 top level 5 path nvme/root-.20261002T1500
   # ...
   ```
   The date is in the name: `root-.20261002T1500` = snapshot of `@` from October 2, 2026, 15:00. The name before the dot is the entry's `snapshot_name` in `/etc/btrbk/btrbk.conf`.

3. **Browse a specific snapshot**:
   ```bash
   # Mount a snapshot read-only
   sudo mkdir -p /mnt/snapshot
   sudo mount -t btrfs -o subvol=nvme/root-.20261002T1500,ro,degraded UUID=<target-uuid> /mnt/snapshot

   # Now browse it like a normal filesystem
   ls /mnt/snapshot/etc/
   ls /mnt/snapshot/home/
   cat /mnt/snapshot/etc/fstab
   ```

4. **Use the GUI file manager** (if in graphical mode):
   - Open Dolphin
   - Navigate to `/mnt/snapshot`
   - Browse, search, and preview files normally

5. **When done, unmount**:
   ```bash
   sudo umount /mnt/snapshot /mnt/backup
   ```

With `btrdasd` installed you can also list a snapshot without a second mount: `btrdasd restore browse /mnt/backup/nvme/root-.20261002T1500 --prefix etc/`.

### Restore a Single File

1. Mount the backup snapshot that contains the file version you want (see above).

2. **Copy the file to the live system**:
   ```bash
   # Example: restore a deleted config file
   sudo cp /mnt/snapshot/etc/important.conf /mnt/broken/etc/important.conf

   # Example: restore a file from home directory
   sudo cp /mnt/snapshot/home/bosco/Documents/thesis.odt /mnt/broken/home/bosco/Documents/

   # Preserve permissions and ownership
   sudo cp -a /mnt/snapshot/path/to/file /mnt/broken/path/to/file
   ```

   On a running system with `btrdasd`, `sudo btrdasd restore file <snapshot-path> <dest-dir> <path-in-snapshot>…` does the same, but only into an allowed root: `[restore] allowed_roots` in `config.toml` (default `/home` and `/tmp`; the author's also grants `/srv/VirtualMachines`), and never under a system path (`/bin`, `/boot`, `/dev`, `/etc`, `/lib`, `/lib64`, `/proc`, `/root`, `/sbin`, `/srv/ftp`, `/srv/http`, `/sys`, `/usr`, `/var/lib`, `/var/spool`), whatever the config says. Restore system files with `cp -a` as above.

3. **To find which snapshot contains a specific file** (if you don't know the date):
   ```bash
   # Search across multiple snapshots
   for snap in /mnt/backup/nvme/root-.*/; do
     if [ -f "${snap}path/to/file" ]; then
       echo "Found in: $snap"
       ls -la "${snap}path/to/file"
     fi
   done
   ```

4. **To search by filename** (if you don't remember the exact path):
   ```bash
   # Search inside a single snapshot
   find /mnt/snapshot -name "thesis*" 2>/dev/null

   # Or use the btrdasd search tool (if installed)
   btrdasd search "thesis"
   ```

### Restore an Entire Subvolume

Use this when you want to roll back an entire subvolume (root, home, etc.) to a previous state.

**Method 1: BTRFS send/receive (fast, preserves BTRFS metadata)**

```bash
# 1. Mount backup drive (top level, by UUID)
sudo mount -t btrfs -o ro,degraded UUID=<target-uuid> /mnt/backup

# 2. Mount the top level of the filesystem you are restoring into
sudo mount -o subvolid=5 /dev/<target-partition> /mnt/target

# 3. Rename the current (broken) subvolume
sudo mv /mnt/target/@ /mnt/target/@.broken

# 4. Send the backup snapshot to the target
#    (This is a fast BTRFS-native operation, not a file copy)
sudo btrfs send /mnt/backup/nvme/root-.20261002T1500 | sudo btrfs receive /mnt/target/

# 5. Make a WRITABLE snapshot of the received one, named @
#    NEVER run `btrfs property set ... ro false` on a received snapshot: it
#    permanently destroys its Received UUID and breaks incremental send/receive.
sudo btrfs subvolume snapshot /mnt/target/root-.20261002T1500 /mnt/target/@

# 6. Keep the read-only received copy until the restore is proven, then delete it
#    sudo btrfs subvolume delete /mnt/target/root-.20261002T1500

# 7. IMPORTANT: Update /etc/fstab in the restored subvolume
#    The backup snapshot's fstab has the UUIDs from when it was taken.
#    If you're restoring to different drives, update the UUIDs.
sudo nano /mnt/target/@/etc/fstab

# 8. Clean up: delete the broken subvolume (when you're sure the restore works)
sudo btrfs subvolume delete /mnt/target/@.broken

# 9. Unmount
sudo umount /mnt/target /mnt/backup
```

**Method 2: rsync (slower but works across filesystem types)**

```bash
# Mount source snapshot and target
sudo mount -t btrfs -o subvol=nvme/root-.20261002T1500,ro,degraded UUID=<backup-target-uuid> /mnt/snapshot
sudo mount -o subvol=@ /dev/<target-partition> /mnt/target

# Sync (--delete removes files that don't exist in the snapshot)
sudo rsync -aAXHv --delete --info=progress2 /mnt/snapshot/ /mnt/target/

# Unmount
sudo umount /mnt/snapshot /mnt/target
```

**Important**: After restoring a root subvolume, always check and update:
- `/etc/fstab` — UUIDs may not match current drives
- Boot entries in `/boot/loader/entries/` — root UUID must be correct
- Regenerate initramfs, with the ESP mounted inside the restored root first — where `/boot` is the ESP (as on the author's system), skipping the mount writes the initramfs into the subvolume's own `/boot` directory, which the bootloader never reads. Method 1 (`/mnt/target` is the top level): `sudo mount LABEL=EFI /mnt/target/@/boot && sudo arch-chroot /mnt/target/@ mkinitcpio -P`. Method 2 (with the target mounted again): `sudo mount LABEL=EFI /mnt/target/boot && sudo arch-chroot /mnt/target mkinitcpio -P`. Use your primary ESP's label; unmount it before unmounting the target

---

## Keeping the recovery OSes current

Each recovery drive boots its own independent OS. It is what you boot when the host cannot, so it has to mount and `btrfs receive` what the host's current kernel and btrfs-progs wrote: a kernel older than a filesystem feature the backups use refuses the mount or the receive. An install left alone for months also stops being able to update itself, because its keyring no longer verifies the packages it is offered.

Every backup run reads each recovery OS (read-only) and adds a `RECOVERY OS` section to the report: install date, last full upgrade and its age (counted from the install when it was never upgraded: `never upgraded since install on <date> (<N> days)`), newest kernel against the host's, btrfs-progs against the host's. Past `[recovery_os].max_age_days` (default 60), with a kernel series behind the host's, or with either unreadable, it says `STALE` and the run status reads `COMPLETED WITH WARNINGS`. `btrdasd health` shows the last reading between runs.

To update one, boot it — never update it from the host. There are two ways:

- **In the `recovery-os-updater` VM**, without rebooting the workstation: the drive's own OS boots in a virtual machine that is lent the whole physical disk — [below](#in-the-recovery-os-updater-vm).
- **On bare metal**: reboot the workstation into the recovery drive — [further below](#on-bare-metal).

Either way, do one drive at a time, so one known-good recovery OS exists throughout. The next backup run reads the new state; `STALE` clears once the last *applied* upgrade is recent and the kernel series and btrfs-progs have caught up with the host's. An upgrade that failed or was declined shows as a `Last attempt … (did not complete)` — the report only counts one whose transaction completed.

**Never update a recovery OS from the host by `arch-chroot` into its `@`.** A kernel upgrade writes that OS's own ESP on the recovery drive, and writing a recovery ESP from the host is exactly what `.claude/rules/esp-safety.md` rule 1 forbids — the 2026-03-05 incident destroyed both recovery boot configurations this way. Booted — on bare metal or in the VM — every write, its ESP included, is made by the drive's own OS, which the rule permits.

### In the recovery-os-updater VM

#### What a session does, and what it guarantees

`recovery-os-vm.sh session` lends one recovery drive to the VM for as long as the VM runs, then gives it back:

1. **The right drive, and only a recovery drive.** It takes the serial that `config.toml` gives that `role = "mirror"` target, finds `/dev/disk/by-id/ata-*_<serial>` (exactly one), requires a whole disk, reads the serial back from the disk itself, and requires partition 2 to carry the filesystem the target is mounted by (`mount_uuid` — a target without one is refused; `sudo btrdasd setup --check` prints the line to add). A serial that also belongs to a `role = "primary"` target is refused whatever the mirror entry says: no drive of the 22 TB pair is ever lent.
2. **Only when nothing else has the drive.** It refuses while `das-backup`, `das-backup-full`, `das-scrub` or `das-backup-doctor` is running, while any partition of the drive is mounted on the host, while the VM is running or already has a disk, and while an earlier session's holder still runs.
3. **Only an OS that will not run btrbk when it boots.** The drive keeps the host's received backups on the same partition 2 its OS boots from, and that OS has btrbk installed: a btrbk run started by its own boot could delete or rewrite them. The nightly backup run records, for each recovery drive, what booting its OS would run (`/var/lib/das-backup/recovery-os.json`), down to one verdict: `will`, `may` or `no`. A session goes on only when that record says `no`, is at most 8 days old, and was made after the drive's last session — the OS may have changed in it; the time of each session is kept in `/var/lib/das-backup/recovery-os-vm-sessions`, written before the OS boots and again when the disk is back. A boot on bare metal leaves no session time, so after one the record still looks current until the next nightly run checks the drive again: run a backup with the drive attached before the next session. Every session shows the record's verdict, its reasons, the enabled units and its age, also when it goes on. `--accept-boot-record-risk` lets a `will` or `may` verdict, an old record or a session since then through — loudly, and into the summary's Warnings line (exit status 5) — but never a record that is missing, cannot be read, is of another schema, or says nothing about this drive.
4. **The maintenance lock**, `/run/das-maintenance.lock`, taken without waiting and held for the whole session. A backup or scrub that starts meanwhile waits for it; the jobs that would otherwise mount a target defer. Its first line names the session and the process holding it — `recovery-os VM session <label> pid <pid>`, the disk holder's pid once it holds — and a refusal elsewhere prints it; it is emptied again before the lock is let go.
5. **The claim.** `btrdasd recovery-os hold-disk` holds the whole disk open exclusively (`O_EXCL`). While it does, the kernel refuses to mount any partition of the drive on the host — by device or by UUID, from any program — while the VM's own non-exclusive open still works. The holder runs in a session of its own (no Ctrl-C or hangup reaches it) and in a systemd scope of its own (`das-recovery-os-holder-<label>.scope`, outside your login session), and it also holds the lock: a closed terminal, a Ctrl-C or a killed script leaves the drive claimed and the backups waiting for as long as the VM may be using it. A holder that dies while the VM runs is replaced at the next check (the VM's own open does not stand in the way), and the summary says so. A session refuses to start without `systemd-run`. Even so, do not stop your user session manager (`loginctl kill-user`, `systemctl stop user@…`) while a session runs. And never `systemctl stop` a `das-recovery-os-holder-*` scope during a session: that ends the claim and the lock at once — a running session claims the disk again at its next check (every 5 seconds), one left in place (exit status 3) does not. A lost claim, even one taken again, ends the session with exit status 5, and the summary says whether it was taken again.
6. **SATA, not virtio.** The whole disk is attached as a SATA disk with boot order 1 — the VM's network card has 2, so the network is tried only after the disk. The recovery OS's initramfs almost certainly lacks the virtio drivers. Its default image finds its root on SATA only if it was built with `ahci` — that is, if the OS was installed on a machine with an AHCI controller. If it was installed with the drive already in the USB enclosure, the default image may carry only the USB storage drivers; the first boot in the VM then needs the **fallback** entry (see below). That is the documented path, not a defect.
7. **It waits** until the recovery OS powers itself off. Reboots inside the VM are part of the job.
8. **Giving it back:** detach the disk, stop the holder, `btrfs device scan` the drive's partition 2 so the host's kernel reads what the other kernel wrote, check that no partition is mounted, release the lock, print a summary.

The host never writes to the drive: no mount, no chroot, no copy. The script never forces off a running recovery OS — it could be in the middle of an update. Interrupted, or out of time, it leaves the VM, the claim and the lock in place and says how to finish (exit status 3). A session that ended but needs a look — the claim was lost while the VM ran, the drive re-enumerated during it, a partition was mounted afterwards, the device scan failed, or the boot-record check was overridden — exits 5 and says why in the summary; a dry run that needed `--accept-boot-record-risk` exits 5 too.

#### Running it

As root. The script is in `/usr/lib/das-backup/` on a `/usr` install, `${prefix}/lib/das-backup/` otherwise; it needs libvirt with KVM, the UEFI firmware (`edk2-ovmf`), libvirt's NAT network `default` and `systemd-run` — see [INSTALL.md](INSTALL.md#recovery-os-vm-optional). Once after installing, and again whenever an upgrade of this project changes the VM's definition (the VM must be shut off, with no disk):

```bash
sudo /usr/lib/das-backup/recovery-os-vm.sh define
```

Then for each drive in turn — `A` and `B` stand for the `role = "mirror"` targets whose labels contain that letter (`system-recovery-A-2tb`, `system-recovery-B-2tb`):

```bash
# Every check, the lock and the claim, taken and given back; nothing attached or booted
sudo /usr/lib/das-backup/recovery-os-vm.sh session A --dry-run

# The real session: boots drive A's own OS and waits until it powers off
sudo /usr/lib/das-backup/recovery-os-vm.sh session A
```

It prints the console to open, from your desktop session. The VM's VNC listens only on a socket libvirt creates for it, which no other user can reach; `--attach` goes through libvirt and its access control:

```bash
virt-viewer --connect qemu:///system --attach recovery-os-updater
```

Other commands:

- `session A --timeout 120` — after 120 minutes, ask the recovery OS to shut down and wait 10 more; it is never forced off. If it is still running then, the session is left in place (exit status 3).
- `session A --accept-boot-record-risk` — only when you know why the boot record says what it says (3 above): the session goes on despite a `will` or `may` verdict, an old record, or a session since the record was made, and its summary says so.
- `status` — the VM's state, the disk it holds, the holder and its scope, and who holds the maintenance lock.
- `screenshot screen.png` — the VM's screen, while it runs (read through QEMU's monitor, so it does not need the VNC socket).
- `session-end A` — finishes a session whose script was interrupted or killed (exit status 3 or 4), once the recovery OS is powered off: detaches, stops the holder (which releases the lock), rescans.

#### The first update, at the VM console

The first time is by hand at the console — it needs your login on the recovery OS. Step 2 also prepares later sessions: virtio drivers let the default boot entry run in a VM on any controller, and the guest agent (step 3) allows the update to be automated.

0. **Look before changing anything.** This OS has not run for months, and it now runs beside the host's backups:

   ```bash
   systemctl list-timers --all            # what will fire while it runs
   ls /etc/pacman.d/hooks/                # an esp-mirror hook here fails harmlessly in the VM (no NVMe ESPs)
   grep -v '^#' /etc/fstab                # a line for something only the workstation has needs nofail
   pacman -Q das-backup-manager 2>/dev/null && systemctl list-unit-files 'das-*'
   ```

   If das-backup-manager is installed here, its timers must not fire in the recovery OS — it is not the host, and its jobs would act on whatever disks it sees. Disable every one that is enabled, inside the recovery OS (an install of another version may have a different set):

   ```bash
   systemctl list-unit-files --no-legend --state=enabled 'das-*.timer' | awk '{print $1}' | xargs -r sudo systemctl disable --now
   ```

   Then a **rollback point**:

   ```bash
   ROOTDEV=$(findmnt -no SOURCE / | sed 's/\[.*//')   # e.g. /dev/sda2 — the drive's own partition 2
   sudo mount -o subvolid=5 "$ROOTDEV" /mnt
   sudo btrfs subvolume snapshot -r /mnt/@ "/mnt/@.pre-update-$(date +%F)"
   sudo umount /mnt
   ```

   It keeps the previous root's files. After the updated OS has booted well — in the VM and on bare metal (step 5) — remove it the same way, so the old packages' blocks are freed:

   ```bash
   sudo mount -o subvolid=5 "$ROOTDEV" /mnt
   sudo btrfs subvolume delete /mnt/@.pre-update-<date>
   sudo umount /mnt
   ```

1. **Keyrings first**, on their own — an install left alone for months cannot verify today's packages without them. If the mirrors it knows are stale, refresh the mirror list first (`sudo pacman -Sy cachyos-mirrorlist` or, when installed, `rate-mirrors`):

   ```bash
   sudo pacman -Sy archlinux-keyring cachyos-keyring
   ```

   If that fails on signatures (an install about six months old can), repair the keyring and try again:

   ```bash
   sudo pacman-key --init                       # only if /etc/pacman.d/gnupg is missing or broken
   sudo pacman-key --populate archlinux cachyos
   sudo pacman -Sy archlinux-keyring cachyos-keyring
   ```

2. **Drivers for both worlds into the initramfs — before the upgrade.** Its default image is built by `autodetect`, which keeps only the drivers for the hardware present *at build time*. Inside the VM that is virtio and SATA, and no USB storage — so any rebuild in the VM, including the one the kernel upgrade in step 3 runs, could drop `usb_storage`, `uas` and the USB host controller's `xhci_pci`/`xhci_hcd`, which this drive needs to boot from the TerraMaster enclosure. Pin both sets first. Find which tool builds the image — `pacman -Qq mkinitcpio dracut 2>/dev/null` prints the one installed.

   - mkinitcpio:

     ```bash
     echo 'MODULES+=(virtio_pci virtio_blk virtio_scsi virtio_net usb_storage uas xhci_pci xhci_hcd ahci)' | sudo tee /etc/mkinitcpio.conf.d/90-virtio.conf
     ```

   - dracut:

     ```bash
     echo 'add_drivers+=" virtio_pci virtio_blk virtio_scsi virtio_net usb_storage uas xhci_pci xhci_hcd ahci "' | sudo tee /etc/dracut.conf.d/90-virtio.conf
     ```

   The same image then boots in the VM and on bare metal; the fallback image, built without autodetection, has everything anyway.

3. **The full upgrade, plus the guest agent** — install nothing else between the keyrings and this: after `-Sy` the package database is newer than the installed packages, a partial upgrade until `-Su` has run. Its kernel hook rebuilds the images with the drivers pinned in step 2:

   ```bash
   sudo pacman -Su qemu-guest-agent
   ```

   Then rebuild every image once more, in case no kernel was upgraded: `sudo mkinitcpio -P` (mkinitcpio), or for dracut the install's own hook — reinstalling the kernel package runs exactly it, with the file names its boot entries expect: `sudo pacman -S linux-cachyos` (repeat for any other kernel `pacman -Qq | grep '^linux-cachyos'` lists). Enable the guest agent: `sudo systemctl enable qemu-guest-agent`. If systemd answers that the unit has no installation config, that is fine — it is then started by udev whenever the VM's agent channel appears.

4. **Reboot inside the VM** (`sudo reboot`), log in again, and check that `uname -r` shows the new kernel — the version `pacman -Q linux-cachyos` reports. Then `sudo poweroff`: the session gives the disk back and prints its summary.

5. **Then boot the drive once on bare metal**, from the enclosure, and confirm its **default** entry boots — the drivers pinned in step 2 are what keep it booting there. Only then count the drive as updated, and remove the rollback point (step 0).

If something goes wrong at the console:

- **The default entry does not find its root** (an emergency shell, or "waiting for root device"): reboot, and at the boot menu pick the **fallback initramfs** entry — it is built without autodetection and carries every storage driver. Expected on the first VM boot of an OS installed with the drive already in the enclosure; step 2 fixes the default entry for next time.
- **The firmware finds no bootloader** ("No bootable option or device was found", or it tries a network (PXE) boot — the network card is second in the boot order — and then lands in its UEFI shell): the VM's firmware starts with no boot entries of its own and looks for `EFI\BOOT\BOOTX64.EFI` on the drive's ESP. Restart the VM — `exit` at the UEFI shell, virt-viewer's Send key → Ctrl+Alt+Del, or `sudo virsh reset recovery-os-updater`, which is harmless while only the firmware runs — press **Esc** during the three-second splash, choose **Boot Maintenance Manager → Boot From File**, pick the drive's ESP and its loader (`EFI/systemd/systemd-bootx64.efi` for systemd-boot). Once booted, `sudo bootctl install` puts systemd-boot at the fallback path as well — the recovery OS writing its own ESP, from inside.
- **An emergency shell over a missing mount**: an `/etc/fstab` line for something only the workstation has, without `nofail`. Add `nofail` to it, from the recovery OS itself.
- **`virsh attach-device` fails with "per-device boot elements cannot be used together with os/boot elements"**: the VM was defined from an older definition, without per-device boot, and libvirt filled in a boot device of its own (`<boot dev='hd'/>` under `<os>`), which refuses the disk's. The boot order is per device: the drive 1, set when a session attaches it, and the network card 2, in the definition. With the VM shut off and no disk attached, run `define` once more, then the session again — the failed attach gave everything back.
- **The session refuses: `btrbk will run when this OS boots`** (or `may`): booting it as it is could start btrbk against the backups on the same partition. Fix it on bare metal from its **emergency** shell, and never let that boot go on. Not its rescue shell: `rescue.target` still starts all of `sysinit.target` — every early service, every `/etc/fstab` mount, and whatever an install enabled under `local-fs.target.wants` or a udev rule — and a runner enabled under exactly such a wants is when the record says `will`. `emergency.target` pulls in nothing but the shell (systemd.special(7): it "does not pull in other services or mounts"; `sysinit.target` conflicts with it).
  1. At its boot menu, edit the default entry's kernel line (systemd-boot: `e`) and add `systemd.unit=emergency.target systemd.setenv=SYSTEMD_SULOGIN_FORCE=1`. The second word gives the root shell even when the root account is locked: `systemd.setenv=` puts the variable in the environment of the emergency shell's unit, which every systemd version passes on; a bare `SYSTEMD_SULOGIN_FORCE=1` on the kernel line is read directly only by newer ones (254 and later, by the review of this guide; this workstation's 262 does).
  2. If `/` is read-only there: `mount -o remount,rw /`.
  3. Mask or disable the runner the record's reasons name — `systemctl mask btrbk.timer btrbk.service`, or remove its link from the `.wants` directory the record names — or move its btrbk config away.
  4. `systemctl poweroff`.

  **Never `exit` or Ctrl-D** at that shell: leaving it goes on to the normal boot, timers included. If you are asked for a password instead ("… root password … or press Control-D to continue"), Ctrl-D boots it **normally**: type the root password, or power off. To power off at any prompt, **hold the power button** (in the VM: the console's force-off) — **never Ctrl-Alt-Del**, which is `reboot.target`, and the next boot takes the default entry.

  Not verified for these drives: the recovery OS's own systemd version, whether its root account is locked, and its systemd-boot `editor` setting (`loader/loader.conf`). With `editor no` the kernel line cannot be edited at the menu: then boot an entry that goes to emergency mode by itself, if the menu has one — the "fallback initramfs" entry is not one, it boots normally — and otherwise do not boot it; ask. The host does not write the recovery OS to fix it. This is advice; the script does none of it.

  Then let the next backup run, with the drive attached, record it again.
- **The session refuses: `cannot tell when this drive's last VM session ended`**: the session-times file, `/var/lib/das-backup/recovery-os-vm-sessions`, is empty or has a line that is not `<label> <seconds>` (a hand edit, or a crash during a write). A new boot record does not help here: the file itself has to be put right. Remove the bad line, or the whole file, then run a backup with the drive attached, so that the record is newer than any session you are unsure of. (A session forced through with `--accept-boot-record-risk` rewrites the file from its well-formed lines and says what it did: another drive's line with no readable time comes back as that drive's line at the session's time, anything else is dropped.)
- **The session refuses: the record is older than 8 days, or was made before this drive's last VM session ended**: let a backup run, with the drive attached, record it again.
- **The session refuses: `no boot record`, `cannot be read`, `is schema N, not 3` or `has no entry`**: the same — the record comes from a backup run that checked this drive, and no flag stands in for it.
- **The session refuses: `Unit das-recovery-os-holder-<label>.scope already exists`**: the scope of an earlier holder is still loaded, usually failed. If `status` shows no session running, clear it with `sudo systemctl reset-failed das-recovery-os-holder-<label>.scope` and run the session again.
- **`virsh start failed: … network 'default' is not active`**: `sudo virsh net-start default && sudo virsh net-autostart default`, then run the session again — the failed start gave everything back.

### On bare metal

1. Boot the recovery drive (Steps 1–5 of [Booting into Rescue Mode](#booting-into-rescue-mode)) and log in.
2. Refresh the keyrings first, on their own:

   ```bash
   sudo pacman -Sy archlinux-keyring cachyos-keyring
   ```

3. Then the full upgrade, straight after:

   ```bash
   sudo pacman -Su
   ```

4. Reboot into the same recovery drive and confirm it comes up — boot menu entry, kernel, login. Only then go back to the host.

---

## Troubleshooting

### "No bootable device" after selecting DAS

**Cause**: UEFI/BIOS cannot find the boot files on the DAS drive.

**Fix**:
1. Try the other DAS boot entry (if you have a mirror recovery drive)
2. Check that DAS is fully powered on and all LEDs are active
3. Try a different USB port (preferably USB 3.0+)
4. In BIOS, disable "Secure Boot" temporarily
5. Verify that the ESP on the DAS drive actually contains bootloader files

### Rescue environment is very slow

**Cause**: USB is slower than internal NVMe/SSD.

**This is normal.** The rescue environment runs from an external USB-attached drive. For faster operation, complete the recovery to internal drives and boot from them.

### "Read-only file system" errors

**Cause**: BTRFS mounted read-only due to errors.

**Fix**:
```bash
# Check the filesystem
sudo btrfs check --readonly /dev/<your-device>

# If errors found and you understand the risks:
sudo btrfs check --repair /dev/<your-device>
# WARNING: --repair can cause data loss. Use only as last resort.
```

### WiFi not working in rescue mode

**Fix**:
1. Use wired ethernet if possible
2. Start NetworkManager:
   ```bash
   sudo systemctl start NetworkManager
   nm-connection-editor  # GUI for WiFi setup (if graphical environment)
   nmcli device wifi connect "<SSID>" password "<password>"  # CLI
   ```

### Cannot find DAS drives

**Fix**:
```bash
# Check if drives are detected
lsblk
dmesg | tail -50 | grep -i "usb\|sd"

# If not detected:
# 1. Reconnect USB cable
# 2. Check DAS power
# 3. Try a different USB port
# 4. Try a different USB cable
```

On the system that runs the backups, the backup targets never appear in a file manager or under `/run/media`: they are hidden from udisks on purpose. Look for them with `lsblk -o NAME,SERIAL,LABEL,UUID` and mount them by UUID. After reconnecting the enclosure, run `sudo btrfs device scan` so the two-drive primary is registered before mounting it.

---

## Reference Information

### Your DAS Drive Serial Numbers

Fill in from `btrdasd config show` or your bay mapping document:

| Role | Serial | Bay |
|------|--------|-----|
| `<role>` | `<serial>` | `<bay>` |
| `<role>` | `<serial>` | `<bay>` |

### Your Important UUIDs

Fill in from `blkid` or your storage architecture document:

| Device | UUID | Purpose |
|--------|------|---------|
| `<device>` | `<uuid>` | `<purpose>` |
| `<device>` | `<uuid>` | `<purpose>` |

### Rescue Environment Credentials

| Field | Value |
|-------|-------|
| Username | `<your-rescue-username>` |
| Password | `<your-rescue-password>` |

### Recommended Recovery Tools

| Tool | Purpose |
|------|---------|
| `gparted` | Graphical partition editor |
| `testdisk` | Partition recovery |
| `ddrescue` | Data recovery from failing drives |
| `smartctl` | Drive health checking |
| `btrfs` | BTRFS filesystem tools |
| `rsync` | File synchronization |

### Useful Commands

```bash
# Check disk health
sudo smartctl -a /dev/<your-device>

# Check BTRFS status
sudo btrfs device stats /mnt/target
sudo btrfs filesystem show

# List block devices with details
lsblk -f

# Mount a backup target read-only, by filesystem UUID (safe)
sudo mount -t btrfs -o ro,degraded UUID=<target-uuid> /mnt/backup

# Check backup snapshot timestamps
ls -la /mnt/backup/<target-subdir>/

# Show configured backup targets
btrdasd config show
```

---

## Getting Help

1. **BTRFS Wiki**: https://btrfs.wiki.kernel.org
2. **Arch Wiki (BTRFS)**: https://wiki.archlinux.org/title/Btrfs
3. **btrbk Documentation**: https://github.com/digint/btrbk
4. **Your distro's support forum** -- for distro-specific recovery steps

---

*Backup system version: 0.7.22.3*
