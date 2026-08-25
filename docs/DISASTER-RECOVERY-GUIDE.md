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
8. [Troubleshooting](#troubleshooting)
9. [Reference Information](#reference-information)

---

## Understanding Your Backup System

### Hardware

- **DAS enclosure**: Your external storage enclosure (any manufacturer, any interface -- USB, Thunderbolt, eSATA)
- **Backup drives**: BTRFS-formatted drives with btrbk snapshot history

### Drive Layout

Your DAS bay mapping (see [DAS-BAY-MAPPING.md](DAS-BAY-MAPPING.md)) documents which bay holds which drive. A typical configuration might include:

- **Bootable recovery drive(s)**: Drives with an ESP + bootable OS installation
- **Primary backup drive**: Large-capacity drive receiving all btrbk snapshots
- **General storage**: Optional expendable-data drives

### What's Backed Up

Your backup targets are defined in `/etc/das-backup/config.toml`. Common categories:

- **System backup**: OS, applications, home folder, system configuration
- **Data backup**: Projects, documents, media source files
- **Recovery drives**: Bootable OS clone that can boot independently from DAS

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
   - **Option 1**: Boot directly from DAS backup (temporary, slow over USB)
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
- Email backup report warns that the array is degraded or that one leg has SMART errors
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
#       devid    1 size 20.01TiB used Y path /dev/sdk1
#       devid    2 size 0 used 0 path MISSING
# (the "MISSING" line — note that devid number)

sudo btrfs device stats /mnt/backup-22tb
# Look for non-zero counters: write_io_errs, read_io_errs, corruption_errs
```

Cross-reference the device serial against your bay map (`docs/examples/author-bay-mapping.md`):
- `ZXA1R71M` (bay 2, devid 2) — RMA replacement for failed `ZXA0LMAE` since 2026-05-15. Note: devid numbering was reversed by the 2026-05-07 `mkfs.btrfs` rebuild — the surviving leg became devid 1.
- `ZXA1NYGZ` (bay 5, devid 1) — was devid 2 prior to 2026-05-07

#### Step 2: Mount the array degraded if it failed to mount

The `backup-run.sh` script always uses degraded mount options, so scheduled backups continue. For interactive use:

```bash
# If /mnt/backup-22tb is not currently mounted
sudo mkdir -p /mnt/backup-22tb
sudo mount -o degraded UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup-22tb

# Or, if udisks2 auto-mounted at /run/media/bosco/das-backup-22tb but failed
# because of degraded state, force the explicit mount:
sudo umount /run/media/bosco/das-backup-22tb 2>/dev/null
sudo mount -o degraded UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup-22tb
```

#### Step 3: Verify SMART on the surviving leg

Before relying on the surviving drive for days while the replacement is sourced and rebuilt:

```bash
sudo smartctl -a -d sat /dev/<surviving-leg>     # Quick attribute view
sudo smartctl -t short -d sat /dev/<surviving-leg>  # 2-min sanity test
# Optional: full extended test (~38h, runs in firmware, no host I/O hit)
sudo smartctl -t long -d sat /dev/<surviving-leg>
```

If the surviving drive shows reallocated sectors or pending sectors, copy the most critical recent snapshots elsewhere immediately — running degraded on a marginal drive is a one-failure-from-data-loss situation.

#### Step 4: Source a replacement drive

- **Required**: equal or larger capacity (≥ 20.01 TiB usable).
- **Recommended**: same model (Seagate ST22000NM000C-3WC103) for matching speed and behavior. Different model is acceptable.
- **Risk hedge**: prefer a drive from a different manufacturing batch than the surviving leg to avoid correlated failure.

When the new drive arrives, run a full SMART extended test (~38 hours) before committing data:

```bash
sudo smartctl -i -d sat /dev/<new-drive>           # Confirm capacity matches
sudo smartctl -t short -d sat /dev/<new-drive>     # 2-min DOA check
sudo smartctl -t long  -d sat /dev/<new-drive>     # 38h extended test
# Wait for completion, then:
sudo smartctl -l selftest -d sat /dev/<new-drive>
# All tests should show "Completed without error"
```

You can begin Step 5 in parallel with the long test — the test runs in the drive's firmware in offline mode and yields to host I/O.

#### Step 5: Power off DAS, swap drive in, power up

Use your bay map to identify the failed drive's bay before pulling. The DAS does not require host shutdown — only DAS power-cycling.

#### Step 6: Partition the new drive identically

The replacement must have a GPT partition that exactly matches the surviving leg's geometry. Replace `/dev/sdNEW` with the new drive's device letter (find via `lsblk -o NAME,SIZE,SERIAL,TRAN`):

```bash
sudo sgdisk --zap-all /dev/sdNEW
sudo sgdisk --new=1:2048:42970644446 --typecode=1:8300 \
    --change-name=1:das-backup-22tb /dev/sdNEW
sudo partprobe /dev/sdNEW
```

Verify with `sudo sgdisk --print /dev/sdNEW` — the partition should be sectors 2048–42970644446, 20.0 TiB, type 8300, name `das-backup-22tb`.

#### Step 7: Replace the failed device in the array

```bash
# Get the missing devid from `btrfs filesystem show` (Step 1)
MISSING_DEVID=<number from "MISSING" line>

# Start the replace — runs in background by default
sudo btrfs replace start "$MISSING_DEVID" /dev/sdNEW1 /mnt/backup-22tb

# Monitor (recover takes ~24-48 hours for ~5 TiB over USB)
watch -n 60 sudo btrfs replace status /mnt/backup-22tb
```

`btrfs replace` reads from the surviving leg, writes to the new device, and updates the superblock. It is online — backups can continue running concurrently (slower).

#### Step 8: Restore RAID-1 across single-profile chunks

Any data that was written while the array was degraded is in `single` profile chunks. Convert them back to RAID-1:

```bash
# `soft` filter only touches chunks that aren't already RAID-1
sudo btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft \
    /mnt/backup-22tb

# Watch progress
sudo btrfs balance status /mnt/backup-22tb
```

#### Step 9: Verify integrity and reset counters

```bash
# Full read of every block on both legs, repairs any checksum mismatches
sudo btrfs scrub start -B /mnt/backup-22tb
sudo btrfs scrub status /mnt/backup-22tb
# "Error summary: no errors found" is what you want

# Confirm all error counters are zero
sudo btrfs device stats /mnt/backup-22tb

# Reset stats to baseline now that the array is healthy
sudo btrfs device stats --reset /mnt/backup-22tb

# Confirm RAID-1 across the board
sudo btrfs filesystem df /mnt/backup-22tb
# Expect: Data, RAID1 / Metadata, RAID1 / System, RAID1 (no `single` lines)
```

#### Step 10: Update bay map and CHANGELOG

Update `docs/examples/author-bay-mapping.md` with the new drive's serial, PARTUUID, and BTRFS UUID_SUB (from `sudo blkid /dev/sdNEW1`). Update CHANGELOG.md to record the replacement date and the failure cause.

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
   lsblk
   ```
   The new drive will show with no partitions.

6. **Partition the new drive** (replace `<new-drive>` with actual device, e.g., `/dev/nvme0n1`):
   ```bash
   # Clone partition table from surviving drive
   sudo sfdisk -d /dev/<surviving-drive> | sudo sfdisk /dev/<new-drive>

   # Or create manually:
   sudo parted /dev/<new-drive> mklabel gpt
   sudo parted /dev/<new-drive> mkpart ESP fat32 1MiB 4GiB
   sudo parted /dev/<new-drive> set 1 esp on
   sudo parted /dev/<new-drive> mkpart primary 4GiB 100%

   # Format ESP
   sudo mkfs.fat -F32 /dev/<new-drive-esp-partition>
   ```

7. **Add the new drive to the BTRFS array**:
   ```bash
   # Mount the existing good drive (if not already mounted)
   sudo mount /dev/<surviving-btrfs-partition> /mnt

   # Add the new drive to the array
   sudo btrfs device add /dev/<new-drive-btrfs-partition> /mnt

   # Start rebalancing to RAID1
   sudo btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt
   ```

8. **Wait for balance to complete** (can take several hours):
   ```bash
   sudo btrfs balance status /mnt
   ```

9. **Copy boot files to new ESP**:
   ```bash
   sudo mkdir -p /mnt/boot
   sudo mount /dev/<new-drive-esp-partition> /mnt/boot
   sudo rsync -aHAXS /boot/ /mnt/boot/
   sudo umount /mnt/boot
   ```

10. **Register UEFI boot entry for the new drive**:
    ```bash
    sudo efibootmgr --create --disk /dev/<new-drive> --part <esp-partition-number> \
      --loader '\EFI\SYSTEMD\SYSTEMD-BOOTX64.EFI' \
      --label "<your-boot-label>" --unicode
    ```

11. **Update fstab** with new UUIDs if needed:
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

5. **Mount the DAS backup**:
   ```bash
   # Find your DAS backup drives
   lsblk | grep sd

   # Mount the backup (use the BTRFS partition, not ESP)
   sudo mkdir -p /mnt/backup
   sudo mount -o subvol=/@ /dev/<your-backup-drive-btrfs-partition> /mnt/backup
   ```

6. **Restore the system**:
   ```bash
   # Create subvolumes matching your original layout
   sudo btrfs subvolume create /mnt/target/@
   sudo btrfs subvolume create /mnt/target/@home
   sudo btrfs subvolume create /mnt/target/@log
   sudo btrfs subvolume create /mnt/target/@root
   # Add any other subvolumes from your configuration

   # Copy root data
   sudo rsync -aAXHv --info=progress2 /mnt/backup/ /mnt/target/@/

   # Mount and restore home (adjust subvolume name for your backup layout)
   sudo mkdir -p /mnt/backup-home
   sudo mount -o subvol=/@home /dev/<your-backup-drive-btrfs-partition> /mnt/backup-home
   sudo rsync -aAXHv --info=progress2 /mnt/backup-home/ /mnt/target/@home/
   ```

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
   # This reinstalls the kernel AND regenerates initramfs

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

Your DAS backup drives contain dated snapshots created by btrbk. Each snapshot is a frozen copy of a subvolume at a specific point in time.

1. **Mount the backup drive** (if not already mounted):
   ```bash
   sudo mkdir -p /mnt/backup
   sudo mount /dev/<backup-partition> /mnt/backup
   ```

2. **List available snapshots**:
   ```bash
   # Show all subvolumes (snapshots are subvolumes)
   sudo btrfs subvolume list /mnt/backup | sort -k9

   # Example output:
   # ID 419 gen 763 top level 5 path nvme/root.20260228T0300
   # ID 420 gen 766 top level 5 path nvme/root.20260302T0828
   # ID 485 gen 1038 top level 5 path nvme/root.20260305T0809
   # ...
   ```
   The date is in the name: `root.20260302T0828` = root snapshot from March 2, 2026, 8:28 AM.

3. **Browse a specific snapshot**:
   ```bash
   # Mount a snapshot read-only
   sudo mkdir -p /mnt/snapshot
   sudo mount -o subvol=nvme/root.20260302T0828,ro /dev/<backup-partition> /mnt/snapshot

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
   sudo umount /mnt/snapshot
   ```

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

3. **To find which snapshot contains a specific file** (if you don't know the date):
   ```bash
   # Search across multiple snapshots
   for snap in /mnt/backup/nvme/root.*/; do
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
# 1. Mount backup drive
sudo mount /dev/<backup-partition> /mnt/backup

# 2. Mount the target where you want to restore
sudo mount /dev/<target-partition> /mnt/target

# 3. Rename the current (broken) subvolume
sudo mv /mnt/target/@ /mnt/target/@.broken

# 4. Send the backup snapshot to the target
#    (This is a fast BTRFS-native operation, not a file copy)
sudo btrfs send /mnt/backup/nvme/root.20260302T0828 | sudo btrfs receive /mnt/target/

# 5. Rename the received snapshot to @
sudo mv /mnt/target/root.20260302T0828 /mnt/target/@

# 6. Make it writable (snapshots are read-only by default)
sudo btrfs property set /mnt/target/@ ro false

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
sudo mount -o subvol=nvme/root.20260302T0828,ro /dev/<backup-partition> /mnt/snapshot
sudo mount -o subvol=@ /dev/<target-partition> /mnt/target

# Sync (--delete removes files that don't exist in the snapshot)
sudo rsync -aAXHv --delete --info=progress2 /mnt/snapshot/ /mnt/target/

# Unmount
sudo umount /mnt/snapshot /mnt/target
```

**Important**: After restoring a root subvolume, always check and update:
- `/etc/fstab` — UUIDs may not match current drives
- Boot entries in `/boot/loader/entries/` — root UUID must be correct
- Regenerate initramfs: `arch-chroot /mnt/target && mkinitcpio -P`

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

# Check backup snapshot timestamps
ls -la /mnt/backup/<snapshot-directory>/

# Mount backup read-only (safe)
sudo mount -o ro,subvol=/@ /dev/<your-backup-partition> /mnt/backup

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

*Backup system version: 0.7.13.2*
