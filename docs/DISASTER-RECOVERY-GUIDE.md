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
5. [Step-by-Step Recovery Procedures](#step-by-step-recovery-procedures)
6. [Troubleshooting](#troubleshooting)
7. [Reference Information](#reference-information)

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

*Backup system version: 0.4.0*
