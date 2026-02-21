# Disaster Recovery Guide

**For CachyOS System Recovery from TerraMaster D6-320 DAS Backup**

This guide is written for users with minimal technical experience. Follow each step exactly as written.

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

Your backup system consists of:

### Hardware
- **TerraMaster D6-320**: 6-bay USB external storage enclosure
- **6x Seagate 2TB drives**: Organized as backup + mirror pairs plus cold spares

### Drive Layout
```
┌───────────────────────────────────┐
│ TerraMaster D6-320 (front view)  │
├───────────┬───────────┬───────────┤
│   Bay 1   │   Bay 2   │   Bay 3   │
│  System   │   Cold    │   Cold    │
│  Mirror   │   Spare   │   Spare   │
├───────────┼───────────┼───────────┤
│   Bay 4   │   Bay 5   │   Bay 6   │
│   Data    │   Data    │  System   │
│  Mirror   │  Backup   │  Backup   │
└───────────┴───────────┴───────────┘
```

### What's Backed Up
- **System Backup (Bay 6 & Bay 1)**: Your OS, applications, and home folder
- **Data Backup (Bay 5 & Bay 4)**: ClaudeCodeProjects and Audiobooks
- **Cold Spares (Bay 2 & Bay 3)**: Ready to replace any failed drive

### Backup Schedule
- **Nightly at 3 AM**: Incremental backup (only changed files)
- **Weekly (every Sunday)**: Full backup refresh

---

## When to Use This Guide

Use this guide when:

1. ❌ Your computer won't boot normally
2. ❌ You see disk errors on startup
3. ❌ Windows/Linux reports "drive not found"
4. ❌ You need to restore files from backup
5. ❌ You're setting up a new/replacement computer

**Important**: If only one NVMe drive fails, your system may still boot normally due to RAID1 protection. The guide covers this scenario too.

---

## Booting into Rescue Mode

### Step 1: Connect the DAS

1. Plug the TerraMaster D6-320 into any USB-C or USB-A 3.0+ port
2. Turn on the DAS using the power button on the back
3. Wait for all drive LEDs to turn solid blue (about 30 seconds)

### Step 2: Enter Boot Menu

1. Restart your computer
2. **Immediately** press the boot menu key repeatedly:
   - Most PCs: **F12**, **F11**, or **F8**
   - ASUS motherboards: **F8**
   - Gigabyte: **F12**
   - MSI: **F11**

3. If you miss it, restart and try again

### Step 3: Select DAS Boot Entry

In the boot menu, look for one of these entries:

```
TerraMaster TDAS (6CAB-B04D)     ← Primary boot drive (Bay 6)
TerraMaster TDAS (6D15-0632)     ← Mirror boot drive (Bay 1)
```

Select either one and press **Enter**.

### Step 4: Choose Rescue Environment

The systemd-boot menu will appear with these options:

```
→ DAS Rescue Environment (XFCE)      ← Choose this for recovery
  CachyOS DAS Recovery (sde)          ← Boots your backed-up system
  CachyOS DAS Recovery (sde) Fallback
```

Select **"DAS Rescue Environment (XFCE)"** and press **Enter**.

### Step 5: Login

- **Username**: `rescue`
- **Password**: `rescue`

You'll see an XFCE desktop with recovery tools.

---

## Recovery Scenarios

### Scenario A: Single NVMe Drive Failure

**Symptoms**: System still boots but shows "degraded array" warnings.

**What to do**:
1. Boot into your normal system (it should still work)
2. Open a terminal and check array status:
   ```bash
   sudo btrfs device stats /
   ```
3. If errors show on one device, replace that NVMe
4. See [Replacing a Failed NVMe Drive](#replacing-a-failed-nvme-drive)

---

### Scenario B: Both NVMe Drives Failed

**Symptoms**: Computer won't boot at all, or BIOS shows "No bootable device".

**What to do**:
1. Boot into Rescue Mode (see [Booting into Rescue Mode](#booting-into-rescue-mode))
2. You can either:
   - **Option 1**: Boot directly from DAS backup (temporary, slow)
   - **Option 2**: Restore backup to new NVMe drives (permanent fix)

See [Full System Restoration](#full-system-restoration) for detailed steps.

---

### Scenario C: Complete System Replacement

**Symptoms**: You have new hardware (new motherboard, CPU, etc.) and need to restore your system.

**What to do**:
1. Install new NVMe drives in the new system
2. Connect the DAS
3. Boot into Rescue Mode
4. Restore backup to new drives
5. Update hardware-specific drivers if needed

See [Restoring to New Hardware](#restoring-to-new-hardware) for detailed steps.

---

## Step-by-Step Recovery Procedures

### Replacing a Failed NVMe Drive

**You will need**: New NVMe drive (same or larger capacity)

**Time required**: About 1-2 hours

1. **Shut down the computer** completely

2. **Replace the failed drive**:
   - Open your computer case
   - Remove the failed NVMe (note which slot: nvme0 or nvme1)
   - Install the new NVMe in the same slot

3. **Boot into Rescue Mode**

4. **Open a terminal** (right-click desktop → Open Terminal Here)

5. **Identify the new drive**:
   ```bash
   lsblk
   ```
   The new drive will show as `nvme0n1` or `nvme1n1` with no partitions

6. **Partition the new drive** (adjust nvmeXn1 to match your new drive):
   ```bash
   # Create GPT partition table
   sudo parted /dev/nvme0n1 mklabel gpt

   # Create ESP partition (4GB)
   sudo parted /dev/nvme0n1 mkpart ESP fat32 1MiB 4GiB
   sudo parted /dev/nvme0n1 set 1 esp on

   # Create main partition (rest of drive)
   sudo parted /dev/nvme0n1 mkpart primary 4GiB 100%

   # Format ESP
   sudo mkfs.fat -F32 /dev/nvme0n1p1
   ```

7. **Add the new drive to the BTRFS array**:
   ```bash
   # Mount the existing good drive
   sudo mount /dev/nvme1n1p2 /mnt

   # Add the new drive to the array
   sudo btrfs device add /dev/nvme0n1p2 /mnt

   # Start rebalancing to RAID1
   sudo btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt
   ```

8. **Wait for balance to complete** (can take several hours):
   ```bash
   sudo btrfs balance status /mnt
   ```

9. **Copy boot files to new ESP**:
   ```bash
   sudo mount /dev/nvme0n1p1 /mnt/boot
   sudo cp /boot/* /mnt/boot/
   ```

10. **Reboot** and test

---

### Full System Restoration

**You will need**: Two new NVMe drives (or repaired existing ones)

**Time required**: About 2-4 hours depending on data size

1. **Boot into Rescue Mode**

2. **Partition both NVMe drives** (repeat for nvme0n1 and nvme1n1):
   ```bash
   # For nvme0n1
   sudo parted /dev/nvme0n1 mklabel gpt
   sudo parted /dev/nvme0n1 mkpart ESP fat32 1MiB 4GiB
   sudo parted /dev/nvme0n1 set 1 esp on
   sudo parted /dev/nvme0n1 mkpart primary 4GiB 100%
   sudo mkfs.fat -F32 /dev/nvme0n1p1

   # Repeat for nvme1n1
   sudo parted /dev/nvme1n1 mklabel gpt
   sudo parted /dev/nvme1n1 mkpart ESP fat32 1MiB 4GiB
   sudo parted /dev/nvme1n1 set 1 esp on
   sudo parted /dev/nvme1n1 mkpart primary 4GiB 100%
   sudo mkfs.fat -F32 /dev/nvme1n1p1
   ```

3. **Create BTRFS RAID1 on main partitions**:
   ```bash
   sudo mkfs.btrfs -m raid1 -d raid1 /dev/nvme0n1p2 /dev/nvme1n1p2
   ```

4. **Mount the new filesystem**:
   ```bash
   sudo mkdir -p /mnt/target
   sudo mount /dev/nvme0n1p2 /mnt/target
   ```

5. **Mount the DAS backup**:
   ```bash
   # Find DAS drives
   lsblk | grep sd

   # Mount backup drive (look for 1.8T drives)
   sudo mkdir -p /mnt/backup
   sudo mount -o subvol=/@ /dev/sde2 /mnt/backup
   ```

6. **Restore the system**:
   ```bash
   # Create subvolumes
   sudo btrfs subvolume create /mnt/target/@
   sudo btrfs subvolume create /mnt/target/@home
   sudo btrfs subvolume create /mnt/target/@log
   sudo btrfs subvolume create /mnt/target/@root

   # Copy data (this takes a while)
   sudo rsync -aAXHv --info=progress2 /mnt/backup/ /mnt/target/@/

   # Mount and restore home
   sudo mount -o subvol=/@home /dev/sde2 /mnt/backup-home 2>/dev/null || \
     sudo mount -o subvol=/@home /mnt/backup /mnt/backup-home
   sudo rsync -aAXHv --info=progress2 /mnt/backup-home/ /mnt/target/@home/
   ```

7. **Install bootloader**:
   ```bash
   # Mount ESP
   sudo mount /dev/nvme0n1p1 /mnt/target/@/boot

   # Chroot and install
   sudo arch-chroot /mnt/target/@
   bootctl install
   exit
   ```

8. **Update fstab with new UUIDs**:
   ```bash
   # Get new UUIDs
   sudo blkid /dev/nvme0n1p1
   sudo blkid /dev/nvme0n1p2

   # Edit fstab
   sudo nano /mnt/target/@/etc/fstab
   # Update UUIDs to match new drives
   ```

9. **Unmount and reboot**:
   ```bash
   sudo umount -R /mnt/target
   sudo reboot
   ```

---

### Restoring to New Hardware

Follow the [Full System Restoration](#full-system-restoration) procedure, then:

1. After first boot, update drivers:
   ```bash
   sudo pacman -Syu
   sudo mkinitcpio -P
   ```

2. If using different GPU, install appropriate drivers:
   ```bash
   # AMD GPU
   sudo pacman -S mesa vulkan-radeon

   # NVIDIA GPU
   sudo pacman -S nvidia nvidia-utils

   # Intel GPU
   sudo pacman -S mesa vulkan-intel
   ```

3. Regenerate initramfs:
   ```bash
   sudo mkinitcpio -P
   ```

4. Reboot

---

## Troubleshooting

### "No bootable device" after selecting DAS

**Cause**: UEFI/BIOS can't find the boot files

**Fix**:
1. Try the other DAS boot entry (mirror drive)
2. Check that DAS is fully powered on
3. Try a different USB port
4. In BIOS, disable "Secure Boot" temporarily

### Rescue environment is very slow

**Cause**: USB is slower than internal NVMe

**This is normal**. The rescue environment runs from USB. For faster operation:
1. Complete recovery to internal drives
2. Boot from internal drives

### "Read-only file system" errors

**Cause**: BTRFS mounted read-only due to errors

**Fix**:
```bash
sudo btrfs check --readonly /dev/sde2
# If errors found:
sudo btrfs check --repair /dev/sde2  # Use with caution!
```

### WiFi not working in rescue mode

**Fix**:
1. Use wired ethernet if possible
2. Start NetworkManager:
   ```bash
   sudo systemctl start NetworkManager
   nm-connection-editor  # GUI for WiFi setup
   ```

### Can't find DAS drives

**Fix**:
```bash
# Check if drives are detected
lsblk
dmesg | tail -50 | grep -i "usb\|sd"

# If not detected, try:
# 1. Reconnect USB cable
# 2. Check DAS power
# 3. Try different USB port
```

---

## Reference Information

### Important UUIDs

| Device | UUID | Purpose |
|--------|------|---------|
| sde1 | 6CAB-B04D | Primary DAS ESP |
| sde2 | 7c7ae72d-09d6-4086-b249-1ac60f21b73b | Primary DAS BTRFS |
| sdh1 | 6D15-0632 | Mirror DAS ESP |
| sdh2 | 60b05268-7f8f-47b5-a38a-752576a1172a | Mirror DAS BTRFS |

### DAS Drive Serial Numbers

| Role | Serial | Bay |
|------|--------|-----|
| System Backup | ZFL41DNY | 6 |
| Data Backup | ZK208Q7J | 5 |
| System Mirror | ZK208Q77 | 1 |
| Data Mirror | ZFL41DV0 | 4 |
| Cold Spare | ZK208RH6 | 3 |
| Cold Spare | ZFL416F6 | 2 |

### Rescue Environment Credentials

- **Username**: `rescue`
- **Password**: `rescue`
- **Root password**: `rescue`

### Installed Recovery Tools

| Tool | Purpose |
|------|---------|
| `gparted` | Graphical partition editor |
| `testdisk` | Partition recovery |
| `ddrescue` | Data recovery from failing drives |
| `smartctl` | Drive health checking |
| `btrfs` | BTRFS filesystem tools |
| `rsync` | File synchronization |
| `firefox` | Web browser (for documentation) |

### Useful Commands

```bash
# Check disk health
sudo smartctl -a /dev/nvme0n1
sudo smartctl -a /dev/sde

# Check BTRFS status
sudo btrfs device stats /mnt/target
sudo btrfs filesystem show

# List block devices
lsblk -f

# Check backup timestamps
ls -la /mnt/backup-system/nvme/

# Mount backup read-only (safe)
sudo mount -o ro,subvol=/@ /dev/sde2 /mnt/backup
```

---

## Emergency Contacts

If you need help beyond this guide:

1. **CachyOS Forum**: https://forum.cachyos.org
2. **Arch Wiki**: https://wiki.archlinux.org
3. **BTRFS Wiki**: https://btrfs.wiki.kernel.org

---

*Last updated: 2026-02-04*
*Backup system version: 2.0.0*
