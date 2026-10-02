# Emergency Recovery Quick Reference

**Print this. Keep it with the DAS enclosure.**

This is the short version. For detailed steps, open `docs/DISASTER-RECOVERY-GUIDE.md` from the DAS-Backup-Manager repository — keep a copy on each recovery drive's desktop, because the install does not put the guides on disk.

---

## How to Boot This Drive

1. Plug in the DAS enclosure and turn it on
2. Restart the computer
3. Press the boot menu key repeatedly during startup:
   - **ASUS**: F8 | **Gigabyte**: F12 | **MSI**: F11 | **Most others**: F12
4. Select the DAS entry (look for "TerraMas" or the drive's serial number: bay 1 `ZK208Q77`, bay 4 `ZFL41DNY`). Never go by an entry number or by the name `UEFI OS` — the firmware gives every disk that name and renumbers entries after any NVRAM reset (the 2026-08-29 board swap did). The recovery ESPs are PARTUUID `fe640619-2c7b-457a-be77-61bc9aff4875` (`RECOV-ESP-1`, bay 1) and `ef19ce6e-de5e-4623-bed0-8717749916b8` (`RECOV-ESP-4`, bay 4)
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
# 1. Mount the primary backup (22TB pair) read-only, by UUID — it keeps the long history
sudo mkdir -p /mnt/backup /mnt/snapshot
sudo mount -t btrfs -o ro,degraded UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup

# 2. List snapshots of / (sorted by date). Layout: <subdir>/<name>.<YYYYMMDDTHHMM>
#    e.g. nvme/root-.20261002T1500 (@), nvme/home.20261002T1500 (@home)
sudo btrfs subvolume list /mnt/backup | grep 'nvme/root-\.' | sort -k9

# 3. Mount a snapshot
sudo mount -t btrfs -o subvol=nvme/root-.20261002T1500,ro,degraded \
    UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/snapshot

# 4. Copy the file you need
sudo cp -a /mnt/snapshot/path/to/file /where/you/want/it

# 5. Clean up
sudo umount /mnt/snapshot /mnt/backup
```

Not there? A subvolume adopted automatically by a backup run is under `/mnt/backup/<first-source>-adopted/` (e.g. `ssd-adopted/`). A subvolume that was deleted is "retired": its last snapshots stay where they were until that target's retention window has passed (kept for 372 days after the retirement date on the primary and deleted by the first `backup-run.sh` run on or after day 373; kept 7 days on the recovery drives, deleted on or after day 8). The 2TB recovery drives hold only the last 7 daily snapshots, and their `@`/`@home` are their own OS, not yours. On the running system, check first that no backup, scrub, reconcile or doctor run holds the DAS: `sudo flock -n /run/das-maintenance.lock true || echo WAIT`.

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

If `/mnt/backup-22tb` is not currently mounted (or won't mount normally) — first make sure no backup, scrub, reconcile or doctor run holds the DAS (`sudo flock -n /run/das-maintenance.lock true || echo WAIT`):
```bash
sudo mkdir -p /mnt/backup-22tb
sudo mount -t btrfs -o noatime,compress=zstd:3,space_cache=v2,autodefrag,commit=120,nossd,degraded \
    UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup-22tb
```

The system's automatic backups already mount by UUID with `degraded` in their mount options, so the next nightly backup runs even with one drive missing. The backup log says so (`journalctl -u das-backup | grep 'RAID-1 degraded'`); the emailed report has no line of its own for it, so do not wait for the email to tell you.

**3. Replace the failed drive**

Power down the DAS, swap in a new 22TB drive of equal or larger capacity (Seagate ST22000NM000C-3WC103 recommended for matching speed), power up.

**4. Find the new drive — by serial, never by letter**
```bash
lsblk -o NAME,SIZE,SERIAL,TRAN
# The new drive will have NO partitions and a different serial than ZXA1R71M / ZXA1NYGZ
NEW=/dev/disk/by-id/ata-ST22000NM000C-3WC103_<new-serial>   # adjust model if different
```

**5. Partition the new drive identically to the surviving one**
```bash
sudo sgdisk --zap-all "$NEW"
sudo sgdisk --new=1:2048:42970644446 --typecode=1:8300 \
    --change-name=1:das-backup-22tb "$NEW"
sudo partprobe "$NEW"
```

**6. Replace the failed device in the BTRFS array**
```bash
# Get the missing devid from `btrfs filesystem show`
sudo btrfs filesystem show /mnt/backup-22tb
# Look for the devid line marked MISSING — note its number
# (today bay 2 ZXA1R71M is devid 2, bay 5 ZXA1NYGZ is devid 1)

# Start the replace (this can take 24-48 hours for 5+ TiB of data over USB)
sudo btrfs replace start <missing-devid> "$NEW"-part1 /mnt/backup-22tb

# Watch progress
sudo btrfs replace status /mnt/backup-22tb
```

**7. After replace completes — scrub, then restore RAID-1 chunks that were written `single` while degraded**
```bash
# Verify integrity first
sudo btrfs scrub start -B /mnt/backup-22tb

# Then move any single-profile chunks back to RAID-1
sudo btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/backup-22tb
sudo btrfs device stats /mnt/backup-22tb   # All counters should be 0
```

**8. Reset error counters, unmount, update the config**
```bash
sudo btrfs device stats --reset /mnt/backup-22tb
sudo umount /mnt/backup-22tb          # the next backup mounts it itself
```
Put the new serial in place of the failed one in `serials` of the `primary-22tb` target in `/etc/das-backup/config.toml`, then `sudo btrdasd setup --upgrade` and `sudo btrdasd setup --check`.

For more detail, see `DISASTER-RECOVERY-GUIDE.md` section "Scenario D: 22TB RAID-1 Backup Array Single-Leg Failure".

### Restore my entire system from backup

See the full guide: `DISASTER-RECOVERY-GUIDE.md` section "Full System Restoration".

Short version:
1. Partition new drives (ESP + BTRFS)
2. `btrfs send` a snapshot (e.g. `nvme/root-.<TS>`) from the **primary** backup (`das-backup-22tb`) to the new drive — never the recovery drives' `@`, which is their own OS
3. Make a writable snapshot of the received one as `@` (`btrfs subvolume snapshot`) — never `btrfs property set … ro false` on a received snapshot
4. Fix fstab UUIDs, reinstall bootloader, regenerate initramfs
5. Reboot

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
| NVMe boot drive (ESP `EFI`, `/boot`) | (root BTRFS: none) | 204445805771 | 20b5fa7e-d8c0-4035-ae45-f80263073a96 |
| NVMe mirror drive (ESP `EFI-BACKUP`) | (root BTRFS: none) | 20465F802394 | 20b5fa7e-d8c0-4035-ae45-f80263073a96 |
| DAS Bay 1 (2TB recovery A, independent OS; ESP `RECOV-ESP-1`) | das-backup-system-recovery-A | ZK208Q77 | 60b05268-7f8f-47b5-a38a-752576a1172a |
| DAS Bay 2 (22TB primary, RAID-1 devid 2) | das-backup-22tb | ZXA1R71M | b2dbe07d-40b9-422e-8ccf-ef4931c40457 |
| DAS Bay 3 | (empty) | — | — |
| DAS Bay 4 (2TB recovery B, independent OS; ESP `RECOV-ESP-4`) | das-backup-system-recovery-B | ZFL41DNY | 7c7ae72d-09d6-4086-b249-1ac60f21b73b |
| DAS Bay 5 (22TB primary, RAID-1 devid 1) | das-backup-22tb | ZXA1NYGZ | b2dbe07d-40b9-422e-8ccf-ef4931c40457 |
| DAS Bay 6 | (empty) | — | — |

> NVMe kernel names (`nvme0n1`, `nvme1n1`) swap between boots like `/dev/sdX` letters do — on 2026-10-02 the boot drive `204445805771` was `nvme1n1`. Match by serial.
>
> **22TB primary is BTRFS RAID-1**: bays 2 + 5 share one filesystem (UUID `b2dbe07d-…`). If one drive fails, mount with `-o degraded`:
> ```bash
> sudo mount -t btrfs -o degraded UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup-22tb
> ```
>
> **Never write to the `RECOV-ESP-*` partitions** — no copy, sync or mirror of your system's ESP onto them. They boot their own OS.

---

## Emergency Contacts & Resources

- **BTRFS Wiki**: https://btrfs.wiki.kernel.org
- **Arch Wiki**: https://wiki.archlinux.org
- **CachyOS**: https://cachyos.org
- **btrbk**: https://github.com/digint/btrbk

---

*Print on both sides. Laminate if possible. Store with the DAS enclosure.*
*Backup system version: 0.7.22.3*
