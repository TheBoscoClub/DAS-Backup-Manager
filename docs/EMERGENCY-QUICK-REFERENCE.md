# Emergency Recovery Quick Reference

**Print this. Keep it with the DAS enclosure.**

This is the short version. For detailed steps, open `docs/DISASTER-RECOVERY-GUIDE.md` from the DAS-Backup-Manager repository — keep a copy on each recovery drive's desktop, because the install does not put the guides on disk.

---

## How to Boot This Drive

1. Plug in the DAS enclosure and turn it on
2. Restart the computer
3. Press the boot menu key repeatedly during startup:
   - **ASUS**: F8 | **Gigabyte**: F12 | **MSI**: F11 | **Most others**: F12
4. Select the DAS entry (look for your enclosure's name or the drive's serial number — fill in below: bay 1 `<serial>`, bay 4 `<serial>`). Never go by an entry number or by the name `UEFI OS` — the firmware gives every disk that name and renumbers entries after any NVRAM reset. The recovery ESPs are identified by PARTUUID: `<PARTUUID-of-recovery-ESP-1>` (`RECOV-ESP-1`, bay 1) and `<PARTUUID-of-recovery-ESP-4>` (`RECOV-ESP-4`, bay 4) — fill in
5. If you still have the last emailed backup report, read this drive's `RECOVERY OS` block: `btrbk at boot: will` means do not boot it bare (see the guide, "The session refuses"); `may` with `btrbk config: none` is safe — the drive's OS carries no btrbk configuration, so nothing on it touches the backups. Its boot menu cannot edit the kernel line (`editor no`): there is no emergency-shell route from the menu
6. At its menu choose **CachyOS Emergency Recovery (Bay 1)** or **(Bay 4)**. The **— Fallback** entry boots the same OS from an initramfs built without hardware autodetection: it carries every driver. The default image is built with autodetection, inside the update VM, so if the default entry stops at an emergency shell on your machine, reboot and choose **— Fallback**. The two **— Previous root, <date> (Bay N)** entries (the second is the Fallback variant; absent until the drive's first upgrade) are a true rollback to the system as it was before the last upgrade: they boot the kernel and initramfs kept from that time, matched to a writable clone of the pre-upgrade root, so the old system comes up as it was, not just its files. Changes made in it are discarded at the next upgrade. If the last upgrade could not refresh them, the older pair stays (titled with its date); if there is none, the one entry **— Previous root UNAVAILABLE: <reason> —** appears, and it boots the current OS, not the old one.
7. The screen says what the disk is doing while it copies the backup index; a countdown appears from the second boot on. Do not power off while it does.

**Login**: username `<your-user>`, password `__________` *(fill in and write on the printout)*

---

## I Need To...

`/dev/<my-nvme-partition>` is the broken system's BTRFS root. If one of its two NVMe drives is dead, mount it with `-o subvol=@,degraded`.

### Reset my root password

```bash
sudo mkdir -p /mnt/broken
sudo mount -o subvol=@ /dev/<my-nvme-partition> /mnt/broken
sudo arch-chroot /mnt/broken   # mounts /dev, /proc, /sys (with EFI variables) and /run itself
passwd root
exit
sudo umount -R /mnt/broken
sudo reboot
```

### Fix /etc/fstab (system won't boot, "emergency mode")

```bash
sudo mkdir -p /mnt/broken
sudo mount -o subvol=@ /dev/<my-nvme-partition> /mnt/broken
sudo nano /mnt/broken/etc/fstab
# Fix or comment out the broken line
sudo umount /mnt/broken
sudo reboot
```

**To find the right UUID**: run `blkid` and match it to the fstab entries.

### Fix a broken bootloader

```bash
sudo mkdir -p /mnt/broken
sudo mount -o subvol=@ /dev/<my-nvme-partition> /mnt/broken
sudo mount /dev/<my-esp-partition> /mnt/broken/boot
sudo arch-chroot /mnt/broken   # not plain chroot: bootctl needs the EFI variables it mounts
bootctl install           # reinstall systemd-boot
mkinitcpio -P             # rebuild all initramfs images
exit
sudo umount -R /mnt/broken
sudo reboot
```

### Stop a service that hangs boot

```bash
sudo mkdir -p /mnt/broken
sudo mount -o subvol=@ /dev/<my-nvme-partition> /mnt/broken
# Remove the service's enable symlink:
sudo rm /mnt/broken/etc/systemd/system/multi-user.target.wants/<service-name>.service
sudo umount /mnt/broken
sudo reboot
```

### Find and restore a deleted file

`<primary-uuid>` below is the UUID of your primary backup filesystem: write it in the cheat
sheet above, or read it with `sudo blkid -s UUID -o value -t LABEL=<primary-label>`.

```bash
# 1. Mount the primary backup (primary pair) read-only, by UUID — it keeps the long history
sudo mkdir -p /mnt/backup /mnt/snapshot
sudo mount -t btrfs -o ro,degraded UUID=<primary-uuid> /mnt/backup

# 2. List snapshots of / (sorted by date). Layout: <subdir>/<name>.<YYYYMMDDTHHMM>
#    e.g. nvme/root-.20261002T1500 (@), nvme/home.20261002T1500 (@home)
sudo btrfs subvolume list /mnt/backup | grep 'nvme/root-\.' | sort -k9

# 3. Mount a snapshot
sudo mount -t btrfs -o subvol=nvme/root-.20261002T1500,ro,degraded \
    UUID=<primary-uuid> /mnt/snapshot

# 4. Copy the file you need
sudo cp -a /mnt/snapshot/path/to/file /where/you/want/it

# 5. Clean up
sudo umount /mnt/snapshot /mnt/backup
```

Not there? A subvolume adopted automatically by a backup run is under `/mnt/backup/<first-source>-adopted/` (e.g. `<volume>-adopted/`). A subvolume that was deleted is "retired": its last snapshots stay where they were until that target's retention window has passed (kept for 372 days after the retirement date on the primary and deleted by the first `backup-run.sh` run on or after day 373; kept 7 days on the recovery drives, deleted on or after day 8). The 2TB recovery drives hold only the last 7 daily snapshots, and their `@`/`@home` are their own OS, not yours. On the running system, check first that no backup, scrub, restore, index, reconcile or doctor run, or recovery-OS VM session, holds the DAS: `sudo flock -n /run/das-maintenance.lock true || echo WAIT` (`cat /run/das-maintenance.lock` shows what the holder recorded).

### One of the two primary backup drives failed (RAID-1 degraded)

The two primary-target drives (bays `<n>` and `<m>`) are a BTRFS RAID-1 pair. If one fails, your data is safe on the surviving drive — but you need to mount it specially and replace the failed drive.

**1. Confirm the failure**
```bash
sudo btrfs filesystem show <primary-label>   # by label: the array is unmounted between backups
# A line saying "*** Some devices missing" means one leg failed
```

**2. Mount the surviving leg (degraded)**

If `/mnt/backup-<primary-label>` is not currently mounted (or won't mount normally) — first make sure no backup, scrub, restore, index, reconcile or doctor run, or recovery-OS VM session, holds the DAS (`sudo flock -n /run/das-maintenance.lock true || echo WAIT`; `cat /run/das-maintenance.lock` shows what the holder recorded):
```bash
sudo mkdir -p /mnt/backup-<primary-label>
sudo mount -t btrfs -o noatime,compress=zstd:3,space_cache=v2,autodefrag,commit=120,nossd,degraded \
    UUID=<primary-uuid> /mnt/backup-<primary-label>
sudo btrfs device stats /mnt/backup-<primary-label>   # a device with non-zero error counters is the failing one
```

The system's automatic backups already mount by UUID with `degraded` in their mount options, so the next nightly backup runs even with one drive missing. The backup log says so (`journalctl -u das-backup -u das-backup-full | grep 'RAID-1 degraded'` — Sunday's full run logs under the second); the emailed report has no line of its own for it, so do not wait for the email to tell you.

**3. Replace the failed drive**

Hold the maintenance lock from here to the end of step 8: a backup run unmounts the array when it ends, even in the middle of a replace. On `WAIT` (a backup, scrub or recovery-OS VM session holds the DAS) never power the DAS off — wait:

```bash
sudo flock -n /run/das-maintenance.lock true || echo WAIT
sudo flock /run/das-maintenance.lock bash   # a root shell holding the lock: do steps 3-8 in it; backups wait
sudo umount /mnt/backup-<primary-label>                 # if it is mounted
```

Power down the DAS, swap in a new drive of equal or larger capacity (ideally the same model, for matching speed), power up, run `sudo btrfs device scan`, and mount the array again with step 2's `mount` command (skip its lock check: you hold the lock).

**4. Find the new drive — by serial, never by letter**
```bash
lsblk -o NAME,SIZE,SERIAL,TRAN
# The new drive will have NO partitions and a different serial than the surviving drive's
NEW=/dev/disk/by-id/ata-<model>_<new-serial>
```

**5. Partition the new drive identically to the surviving one** (`sgdisk` is in the `gptfdisk` package: `sudo pacman -S gptfdisk` if it is missing)
```bash
sudo sgdisk --zap-all "$NEW"
sudo sgdisk --new=1:2048:<end-sector-of-the-surviving-drive's-partition> --typecode=1:8300 \
    --change-name=1:<primary-label> "$NEW"
sudo partprobe "$NEW"
```

**6. Replace the failed device in the BTRFS array**
```bash
# Get the missing devid from `btrfs filesystem show`
sudo btrfs filesystem show <primary-label>
# Look for the devid line marked MISSING — note its number
# (note which bay each devid is — fill in)

# Replace, in the foreground: returns when done (24-48 hours for 5+ TiB over USB)
sudo btrfs replace start -B <missing-devid> "$NEW"-part1 /mnt/backup-<primary-label>

# Watch progress from another terminal
sudo btrfs replace status -1 /mnt/backup-<primary-label>
```

**7. After replace completes — scrub, then restore RAID-1 chunks that were written `single` while degraded**
```bash
# Verify integrity first
sudo btrfs scrub start -B /mnt/backup-<primary-label>

# Then move any single-profile chunks back to RAID-1
sudo btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/backup-<primary-label>
sudo btrfs device stats /mnt/backup-<primary-label>   # All counters should be 0
```

**8. Reset error counters, unmount, update the config**
```bash
sudo btrfs device stats --reset /mnt/backup-<primary-label>
sudo umount /mnt/backup-<primary-label>          # the next backup mounts it itself
exit                                  # leave the lock shell from step 3: backups go on
```
Put the new serial in place of the failed one in `serials` of the `primary-22tb` target in `/etc/das-backup/config.toml`, then `sudo btrdasd setup --upgrade` and `sudo btrdasd setup --check`.

For more detail, see `DISASTER-RECOVERY-GUIDE.md` section "Scenario D: RAID-1 Backup Array Single-Leg Failure".

### Restore my entire system from backup

See the full guide: `DISASTER-RECOVERY-GUIDE.md` section "Full System Restoration".

Short version:
1. Partition new drives (ESP + BTRFS)
2. `btrfs send` a snapshot (e.g. `nvme/root-.<TS>`) from the **primary** backup (`<primary-label>`) to the new drive — never the recovery drives' `@`, which is their own OS
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
| Boot drive (ESP `EFI`, `/boot`) | | | |
| Mirror drive (ESP mirror, if you keep one) | | | |
| DAS bay 1 (recovery A, independent OS; ESP `RECOV-ESP-1`) | | | |
| DAS bay 2 (primary, RAID-1 devid ?) | | | |
| DAS bay 3 | | | |
| DAS bay 4 (recovery B, independent OS; ESP `RECOV-ESP-4`) | | | |
| DAS bay 5 (primary, RAID-1 devid ?) | | | |
| DAS bay 6 | | | |

> NVMe kernel names (`nvme0n1`, `nvme1n1`) swap between boots like `/dev/sdX` letters do — a boot drive can be `nvme0n1` one day and `nvme1n1` the next. Match by serial.
>
> **The primary target is BTRFS RAID-1**: its two bays share one filesystem (UUID `<primary-uuid>`). If one drive fails, mount with `-o degraded`:
> ```bash
> sudo mount -t btrfs -o degraded UUID=<primary-uuid> /mnt/backup-<primary-label>
> ```
>
> **Never write to the `RECOV-ESP-*` partitions** — no copy, sync or mirror of your system's ESP onto them. They boot their own OS.

---

## Emergency Contacts & Resources

- **BTRFS documentation**: https://btrfs.readthedocs.io
- **Arch Wiki**: https://wiki.archlinux.org
- **CachyOS**: https://cachyos.org
- **btrbk**: https://github.com/digint/btrbk

---

*Print on both sides. Laminate if possible. Store with the DAS enclosure.*
*Backup system version: 0.7.23.1*
