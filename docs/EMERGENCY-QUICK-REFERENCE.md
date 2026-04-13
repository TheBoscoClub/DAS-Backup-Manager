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
| DAS Bay 1 (2TB mirror) | das-backup-system-mirror | ZK208Q77 | |
| DAS Bay 2 (22TB primary) | das-backup-22tb | ZXA0LMAE | |
| DAS Bay 3 (2TB system) | das-backup-system | ZFL41DNY | |

---

## Emergency Contacts & Resources

- **BTRFS Wiki**: https://btrfs.wiki.kernel.org
- **Arch Wiki**: https://wiki.archlinux.org
- **CachyOS**: https://cachyos.org
- **btrbk**: https://github.com/digint/btrbk

---

*Print on both sides. Laminate if possible. Store with the DAS enclosure.*
*Backup system version: 0.7.12*
