# Emergency Recovery Quick Reference

**Print this. Keep it with the DAS enclosure.**

This is the short version. For detailed steps, open `DISASTER-RECOVERY-GUIDE.md` on this drive's desktop or in `/usr/share/das-backup/docs/`.

---

## How to Boot This Drive

1. Plug in the DAS enclosure and turn it on
2. Restart the computer
3. Press the boot menu key repeatedly during startup:
   - **ASUS**: F8 | **Gigabyte**: F12 | **MSI**: F11 | **Most others**: F12
4. Select the DAS entry (look for "TerraMas" or the drive's serial number)
5. At the bootloader menu, select **CachyOS (Fallback Initramfs)** for maximum hardware compatibility

**Login**: username `bosco`, password `__________` *(fill in and write on the printout)*

---

## I Need To...

### Reset my root password

```bash
sudo mount -o subvol=@ /dev/<my-nvme-partition> /mnt/broken
sudo mount --bind /dev /mnt/broken/dev
sudo mount --bind /proc /mnt/broken/proc
sudo mount --bind /sys /mnt/broken/sys
sudo chroot /mnt/broken
passwd root
exit
sudo umount -R /mnt/broken
sudo reboot
```

### Fix /etc/fstab (system won't boot, "emergency mode")

```bash
sudo mount -o subvol=@ /dev/<my-nvme-partition> /mnt/broken
sudo nano /mnt/broken/etc/fstab
# Fix or comment out the broken line
sudo umount /mnt/broken
sudo reboot
```

**To find the right UUID**: run `blkid` and match it to the fstab entries.

### Fix a broken bootloader

```bash
sudo mount -o subvol=@ /dev/<my-nvme-partition> /mnt/broken
sudo mount /dev/<my-esp-partition> /mnt/broken/boot
sudo mount --bind /dev /mnt/broken/dev
sudo mount --bind /proc /mnt/broken/proc
sudo mount --bind /sys /mnt/broken/sys
sudo chroot /mnt/broken
bootctl install           # reinstall systemd-boot
mkinitcpio -P             # rebuild all initramfs images
exit
sudo umount -R /mnt/broken
sudo reboot
```

### Stop a service that hangs boot

```bash
sudo mount -o subvol=@ /dev/<my-nvme-partition> /mnt/broken
# Remove the service's enable symlink:
sudo rm /mnt/broken/etc/systemd/system/multi-user.target.wants/<service-name>.service
sudo umount /mnt/broken
sudo reboot
```

### Find and restore a deleted file

```bash
# 1. Mount the DAS backup drive
sudo mount /dev/<backup-partition> /mnt/backup

# 2. List snapshots (sorted by date)
sudo btrfs subvolume list /mnt/backup | grep "root\." | sort -k9

# 3. Mount a snapshot
sudo mount -o subvol=nvme/root.20260302T0828,ro /dev/<backup-partition> /mnt/snapshot

# 4. Copy the file you need
sudo cp /mnt/snapshot/path/to/file /where/you/want/it

# 5. Clean up
sudo umount /mnt/snapshot /mnt/backup
```

### One of the two 22TB backup drives failed (RAID-1 degraded)

The two 22TB drives in bays 2 and 5 are a BTRFS RAID-1 pair. If one fails, your data is safe on the surviving drive — but you need to mount it specially and replace the failed drive.

**1. Confirm the failure**
```bash
sudo btrfs filesystem show /mnt/backup-22tb
# A line saying "*** Some devices missing" means one leg failed
sudo btrfs device stats /mnt/backup-22tb
# Look for a device with non-zero error counters
```

**2. Mount the surviving leg (degraded)**

If `/mnt/backup-22tb` is not currently mounted (or won't mount normally):
```bash
sudo mkdir -p /mnt/backup-22tb
sudo mount -o degraded UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup-22tb
```

The system's automatic backups already use `degraded` in their mount options, so the next nightly backup should run even with one drive missing. Email reports will warn loudly about the degraded state.

**3. Replace the failed drive**

Power down the DAS, swap in a new 22TB drive of equal or larger capacity (Seagate ST22000NM000C-3WC103 recommended for matching speed), power up.

**4. Find the new drive's letter**
```bash
lsblk -o NAME,SIZE,SERIAL,TRAN
# The new drive will have NO partitions and a different serial than ZXA1R71M / ZXA1NYGZ
```

**5. Partition the new drive identically to the surviving one**
```bash
# Replace /dev/sdNEW with the new drive's letter
sudo sgdisk --zap-all /dev/sdNEW
sudo sgdisk --new=1:2048:42970644446 --typecode=1:8300 \
    --change-name=1:das-backup-22tb /dev/sdNEW
sudo partprobe /dev/sdNEW
```

**6. Replace the failed device in the BTRFS array**
```bash
# Get the missing devid from `btrfs filesystem show`
sudo btrfs filesystem show /mnt/backup-22tb
# Look for "devid X size Y path /dev/sd?1 MISSING" — note X

# Start the replace (this can take 24-48 hours for 5+ TiB of data over USB)
sudo btrfs replace start <missing-devid> /dev/sdNEW1 /mnt/backup-22tb

# Watch progress
sudo btrfs replace status /mnt/backup-22tb
```

**7. After replace completes — restore RAID-1 chunks that were written `single` while degraded**
```bash
# This balance moves any single-profile chunks back to RAID-1
sudo btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/backup-22tb

# Then verify integrity
sudo btrfs scrub start -B /mnt/backup-22tb
sudo btrfs device stats /mnt/backup-22tb   # All counters should be 0
```

**8. Reset error counters and remount normally**
```bash
sudo btrfs device stats --reset /mnt/backup-22tb
sudo umount /mnt/backup-22tb
sudo mount UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup-22tb
```

For more detail, see `DISASTER-RECOVERY-GUIDE.md` section "Recovery: 22TB RAID-1 Backup Array Single-Leg Failure".

### Restore my entire system from backup

See the full guide: `DISASTER-RECOVERY-GUIDE.md` section "Full System Restoration".

Short version:
1. Partition new drives (ESP + BTRFS)
2. `btrfs send` a snapshot from the backup drive to the new drive
3. Fix fstab UUIDs, reinstall bootloader, regenerate initramfs
4. Reboot

---

## Finding Your Drives

```bash
# Show all drives with UUIDs and labels
lsblk -f

# Show drives with serial numbers
lsblk -o NAME,SIZE,MODEL,SERIAL,TRAN

# Show BTRFS filesystems
sudo btrfs filesystem show
```

### Drive Cheat Sheet

*(Fill in your actual values and keep this current)*

| Role | Label | Serial | UUID |
|------|-------|--------|------|
| NVMe 1 (boot) | | | |
| NVMe 2 (mirror) | | | |
| DAS Bay 1 (2TB recovery A, independent) | das-backup-system-recovery-A | ZK208Q77 | 60b05268-7f8f-47b5-a38a-752576a1172a |
| DAS Bay 2 (22TB primary, RAID-1 leg 1) | das-backup-22tb | ZXA1R71M | b2dbe07d-40b9-422e-8ccf-ef4931c40457 |
| DAS Bay 3 | (empty) | — | — |
| DAS Bay 4 (2TB recovery B, independent) | das-backup-system-recovery-B | ZFL41DNY | 7c7ae72d-09d6-4086-b249-1ac60f21b73b |
| DAS Bay 5 (22TB primary, RAID-1 leg 2) | das-backup-22tb | ZXA1NYGZ | b2dbe07d-40b9-422e-8ccf-ef4931c40457 |
| DAS Bay 6 | (empty) | — | — |

> **22TB primary is BTRFS RAID-1**: bays 2 + 5 share one filesystem (UUID `b2dbe07d-…`). If one drive fails, mount with `-o degraded`:
> ```bash
> sudo mount -o degraded UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup
> ```

---

## Emergency Contacts & Resources

- **BTRFS Wiki**: https://btrfs.wiki.kernel.org
- **Arch Wiki**: https://wiki.archlinux.org
- **CachyOS**: https://cachyos.org
- **btrbk**: https://github.com/digint/btrbk

---

*Print on both sides. Laminate if possible. Store with the DAS enclosure.*
*Backup system version: 0.7.13.1*
