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
1. Boot into your normal system on the surviving mirror. A BTRFS RAID-1 root with a device missing mounts only with `degraded`, and on the author's system the default boot entry does not carry it: at the boot menu choose **CachyOS (Safe Mode)**, **CachyOS (Fallback Initramfs)** or **CachyOS (CLI Only)**, whose `rootflags=subvol=/@,degraded` let it mount
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
- The backup log (`journalctl -u das-backup -u das-backup-full` — the Sunday full run logs under the second) says `primary-22tb: present=[…] missing=[…] — RAID-1 degraded, proceeding`. As of `backup-run.sh` v4.12.0 the emailed report has no line of its own for this and its SMART section shows one serial per target, so a clean-looking email does not prove both legs are present
- `sudo btrfs filesystem show das-backup-22tb` says `*** Some devices missing` — ask by label: between backups the array is not mounted, and `btrfs filesystem show /mnt/backup-22tb` then answers only `not a valid btrfs filesystem`
- Once it is mounted (Step 2), `sudo btrfs device stats /mnt/backup-22tb` shows non-zero error counters on one leg

**Why this is a separate scenario**: This array is not in `/etc/fstab` and has nothing to do with system boot. The system continues booting and running normally on its NVMe RAID-1. What needs recovery is the *backup target itself* — so that incremental backups, restores, and disaster-recovery procedures keep working during the days it takes to replace a 22TB drive.

#### Why backups still work in degraded mode

`/etc/das-backup/config.toml` sets `[das].mount_opts` to include `degraded`. The `backup-run.sh` script mounts the target with these options, so a missing leg does not abort the nightly backup. The downside: **any data written while degraded is allocated as `single` profile** (not redundant). After the failed leg is replaced, a balance restores RAID-1 across all chunks. Until then, only one copy of recent data exists.

#### Step 1: Confirm which leg failed

```bash
sudo btrfs filesystem show das-backup-22tb
# Output looks like:
#   Label: 'das-backup-22tb' uuid: b2dbe07d-40b9-422e-8ccf-ef4931c40457
#       Total devices 2 FS bytes used X.XTiB
#       devid    1 size 20.01TiB used Y path /dev/sdX1
#       devid    2 size 0 used 0 path MISSING
# (the "MISSING" line — note that devid number)
```

The error counters need the array mounted: they come after Step 2.

Cross-reference the device serial against your bay map (`docs/examples/author-bay-mapping.md`):
- `ZXA1R71M` (bay 2, devid 2) — RMA replacement for failed `ZXA0LMAE` since 2026-05-15. Note: devid numbering was reversed by the 2026-05-07 `mkfs.btrfs` rebuild — the surviving leg became devid 1.
- `ZXA1NYGZ` (bay 5, devid 1) — was devid 2 prior to 2026-05-07

#### Step 2: Mount the array degraded if it failed to mount

The `backup-run.sh` script always mounts by UUID with `[das].mount_opts` (which include `degraded`), so scheduled backups continue. For interactive use, mount the same way:

```bash
# Is a backup, scrub, restore, index, reconcile or doctor run, or a recovery-OS
# VM session, holding the DAS? The runs mount and unmount this target themselves
# (a VM session holds a drive), and all hold this lock while they do;
# `cat /run/das-maintenance.lock` shows what the holder recorded.
sudo flock -n /run/das-maintenance.lock true || echo "WAIT"

# If /mnt/backup-22tb is not currently mounted
sudo mkdir -p /mnt/backup-22tb
sudo mount -t btrfs -o noatime,compress=zstd:3,space_cache=v2,autodefrag,commit=120,nossd,degraded \
    UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457 /mnt/backup-22tb

# Now that it is mounted: look for non-zero counters
# (write_io_errs, read_io_errs, corruption_errs)
sudo btrfs device stats /mnt/backup-22tb
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

Do not power-cycle the DAS (Step 5) while a SMART test runs on any drive inside it — the surviving leg's from Step 3, or this one's if you test it in a bay: cutting the power aborts the test. `sudo smartctl -c -d sat <drive>` shows its `Self-test execution status`; wait until no test is in progress. The test runs in the drive's firmware and yields to host I/O, so backups can go on meanwhile.

#### Step 5: Power off DAS, swap drive in, power up

Use your bay map to identify the failed drive's bay before pulling. The DAS does not require host shutdown — only DAS power-cycling.

Nothing may be using the DAS while it is off, and a backup run unmounts the array when it ends — even in the middle of the replace in Step 7. So take the maintenance lock now and hold it until the end of Step 9: in a terminal that will stay open for the next two days (closing it lets the lock go), open a root shell that holds it, and do Steps 5–9 in that shell (what you set before does not carry into it: set `NEW` there again, as in Step 4). Check first: `WAIT` means a backup, scrub or recovery-OS VM session holds the DAS — never power it off then; wait, and check again.

```bash
sudo flock -n /run/das-maintenance.lock true || echo WAIT
sudo flock /run/das-maintenance.lock bash   # a root shell holding the lock; backups and scrubs due meanwhile wait
sudo umount /mnt/backup-22tb                 # if you mounted it in Step 2
```

Then power the DAS off, swap the drive, and power it on. Once it is up, run `sudo btrfs device scan` and mount the array again with Step 2's `mount` command (skip its lock check: you hold the lock).

#### Step 6: Partition the new drive identically

The replacement must have a GPT partition that exactly matches the surviving leg's geometry. Address it by its serial (`$NEW` from Step 4) — `--zap-all` on the wrong drive destroys it, and `/dev/sdX` letters move on every reconnect. Confirm with `lsblk -o NAME,SIZE,SERIAL,TRAN` that the serial is the new drive's and that it has no partitions. `sgdisk` is in the `gptfdisk` package; install it (`sudo pacman -S gptfdisk`) if it is missing:

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

# Replace in the foreground (-B): the command returns when the replace is done,
# ~24-48 hours for ~5 TiB over USB — only then go on to Step 8
sudo btrfs replace start -B "$MISSING_DEVID" "$NEW"-part1 /mnt/backup-22tb

# Monitor from another terminal (-1 prints once, so watch can repeat it)
watch -n 60 sudo btrfs replace status -1 /mnt/backup-22tb
```

`btrfs replace` reads from the surviving leg, writes to the new device, and updates the superblock. The filesystem stays usable while it runs, but no backup may run meanwhile: a backup run unmounts the array when it ends (Step 2). That is why you hold the maintenance lock until Step 9 — a backup due meanwhile waits for it, and runs afterwards.

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

Unmount it when you are done (`sudo umount /mnt/backup-22tb`), then `exit` the root shell from Step 5 to let the maintenance lock go: the backups that waited run now, and Step 10's `btrdasd setup` refuses while the lock is held. The next backup mounts the array itself.

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

   # Rebuild the missing copy onto the new partition, in the foreground (-B),
   # so the balance below starts only once the replace is done
   sudo btrfs replace start -B <missing-devid> /dev/<new-drive-btrfs-partition> /mnt

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
   # Label the ESP as your fstab and ESP mirroring expect (author's: EFI on the
   # first drive, EFI-BACKUP on the second) — fstab mounts /boot by LABEL=EFI
   sudo mkfs.fat -F32 -n <esp-label> /dev/<drive-esp-partition>
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
   The subvolume names above are the author's (`btrdasd subvol list` shows yours); a snapshot holds an empty directory where a nested subvolume sat, so restore each nested one from its own snapshot. The author's `@/@audiobooks-db` is snapshotted as `nvme/-audiobooks-db.<TIMESTAMP>`: a name that begins with `-` is read as an option by most commands, so always give it as a full path (`/mnt/backup/nvme/-audiobooks-db.<TIMESTAMP>/`), or put `--` before it.

7. **Install bootloader**:
   ```bash
   # Mount ESP
   sudo mount /dev/<drive-esp-partition> /mnt/target/@/boot

   # Chroot and install bootloader
   sudo arch-chroot /mnt/target/@    # Arch/CachyOS
   # Or for Debian/Ubuntu: sudo chroot /mnt/target/@

   bootctl install                    # For systemd-boot
   # Or: grub-install /dev/<drive>    # For GRUB

   # The new ESP is empty: nothing has put a kernel, initramfs or microcode on it
   # (mkinitcpio -P alone fails without the kernel image). Install them in ONE
   # transaction — amd-ucode owns /boot/amd-ucode.img, not the kernel package —
   # with every kernel you had (pacman -Qq | grep '^linux-cachyos'):
   pacman -S amd-ucode linux-cachyos
   sdboot-manage gen                  # writes the loader entries
   exit
   ```

   Then check the loader entries: every `root=UUID=` in `/mnt/target/@/boot/loader/entries/*.conf` must be the new BTRFS filesystem's UUID (`sudo blkid /dev/<drive1-btrfs-partition>`); correct any that is not. Put back by hand, with the new UUID, every entry no generator writes (the author's Safe Mode and CLI entries: `examples/author-storage-reference.md` §2a).

8. **Update fstab with new UUIDs**:
   ```bash
   # Get new UUIDs
   sudo blkid /dev/<drive-esp-partition>
   sudo blkid /dev/<drive-btrfs-partition>

   # Edit fstab in the restored system
   sudo nano /mnt/target/@/etc/fstab
   # Replace old UUIDs with new ones
   ```

   Also change every line that mounts a subvolume by `subvolid=`: a restored subvolume is a new one with a new id, and a line naming the old id stops the boot in emergency mode. Name it by path instead. On the author's system `/var/lib/audiobooks/db` is mounted with `subvolid=2816`; after a restore that line must say `subvol=/@/@audiobooks-db`.

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

# 2. Mount it (with one drive of a RAID-1 root missing: -o subvol=@,degraded)
sudo mkdir -p /mnt/broken
sudo mount -o subvol=@ /dev/<broken-system-partition> /mnt/broken

# 3. If you also need to fix boot files, mount the ESP
sudo mount /dev/<broken-system-esp> /mnt/broken/boot

# 4. For operations that need a running system (mkinitcpio, passwd, systemctl,
#    bootctl), enter it with arch-chroot. It mounts /dev, /proc, /sys — with the
#    EFI variables (efivarfs) bootctl needs — and /run, and takes them down on exit:
sudo arch-chroot /mnt/broken
#    Without arch-chroot (package arch-install-scripts), bind them recursively:
#    a plain --bind of /sys leaves efivarfs out, and bootctl cannot write its entry.
#      for d in dev proc sys run; do sudo mount --rbind /$d /mnt/broken/$d && sudo mount --make-rslave /mnt/broken/$d; done
#      sudo chroot /mnt/broken
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
   # The broken system's journal, read from outside the chroot: its boots,
   # newest last, then the errors of its last boot (-b -1 for the one before)
   sudo journalctl -D /mnt/broken/var/log/journal --list-boots
   sudo journalctl -D /mnt/broken/var/log/journal -b -0 -p err
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
#    A nested subvolume is not in the snapshot (an empty directory stands where
#    it sat): restore it from its own snapshot, and mount it by path, never by
#    subvolid= — a restored subvolume has a new id, and a line naming the old one
#    stops the boot in emergency mode (author's: /var/lib/audiobooks/db,
#    subvolid=2816 must become subvol=/@/@audiobooks-db).
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

Every backup run reads each recovery OS (read-only) and adds a `RECOVERY OS` section to the report: install date, last full upgrade and its age (counted from the install when it was never upgraded: the `Age` row then reads `<N> days since install`), newest kernel against the host's, btrfs-progs against the host's — and what booting it would start: `Enabled timers` (by unit tree), `btrbk config` (btrbk's default config, present or absent) and `btrbk at boot` (`will`, `may` or `no`; for `no` the row says what was ruled out, while a `will` or `may` adds a `WARNING` row that carries its reasons). It says `STALE`, with its reasons, when the last applied upgrade is older than `[recovery_os].max_age_days` (default 60) — or, never upgraded, the install is: `STALE — never upgraded since install on <date> (<N> days)` — or cannot be dated; when a later upgrade attempt did not complete; when its newest kernel's series is behind the host's, or either kernel is unknown; or when its btrfs-progs is older than the host's. That or a `WARNING` makes the run status read `COMPLETED WITH WARNINGS`. `btrdasd health` shows the last reading between runs. The reading is kept in `/var/lib/das-backup/recovery-os.json`, each drive's with the UUID of the filesystem it was read from (`mount_uuid`).

**Before booting a recovery OS anywhere — in the VM or on bare metal — read its `RECOVERY OS` block** in the last backup report (or `btrdasd health`): `Enabled timers`, `btrbk at boot` and any `WARNING`. If btrbk would run at boot, disable `btrbk.timer` (and whatever else the reasons name) from bare metal first, from its emergency shell — see "A record says `btrbk will run when this OS boots`" below — or in an attended VM session, where the session guard keeps it from running while you disable it. The VM session reads the same record itself: attended it boots a `will` with a banner, under the guard; `--unattended` refuses it. On bare metal nothing protects the backups.

To update one, boot it — never update it from the host. There are two ways:

- **In the `recovery-os-updater` VM**, without rebooting the workstation: the drive's own OS boots in a virtual machine of its own — one per recovery drive — that is lent the whole physical disk, by hand at its console or unattended through its guest agent — [below](#in-the-recovery-os-updater-vm).
- **On bare metal**: reboot the workstation into the recovery drive — [further below](#on-bare-metal).

Either way, do one drive at a time, so one known-good recovery OS exists throughout. The next backup run reads the new state; `STALE` clears once the last *applied* upgrade is recent, no later attempt failed, and the kernel series and btrfs-progs have caught up with the host's. An upgrade that failed or was declined shows as a `Last attempt … (did not complete)` — the report only counts one whose transaction completed.

**Never update a recovery OS from the host by `arch-chroot` into its `@`.** A kernel upgrade writes that OS's own ESP on the recovery drive, and writing a recovery ESP from the host is exactly what `.claude/rules/esp-safety.md` rule 1 forbids — the 2026-03-05 incident destroyed both recovery boot configurations this way. Booted — on bare metal or in the VM — every write, its ESP included, is made by the drive's own OS, which rule 4 of the same file permits.

### In the recovery-os-updater VM

#### What a session does, and what it guarantees

Each recovery drive has a VM of its own: `recovery-os-updater-<label>` (`recovery-os-updater-system-recovery-A-2tb`, `recovery-os-updater-system-recovery-B-2tb`), each with its own firmware variables (NVRAM, which keep the boot entries its drive's firmware learned), network address, console, log and guard channel. `define` makes them all from the one template, with names and identities derived from the `config.toml` labels, and retires the single shared `recovery-os-updater` of earlier releases (only when it is shut off with no disk, no guard left in it, no managed-save image and no libvirt snapshots; its NVRAM goes with it). Wherever this section says "the VM", it is the drive's own.

`recovery-os-vm.sh session` lends one recovery drive to its VM for as long as the VM runs, then gives it back:

1. **The right drive, and only a recovery drive.** It takes the serial that `config.toml` gives that `role = "mirror"` target, finds `/dev/disk/by-id/ata-*_<serial>` (exactly one), requires a whole disk, reads the serial back from the disk itself, and requires partition 2 to carry the filesystem the target is mounted by (`mount_uuid` — a target without one is refused; `sudo btrdasd setup --check` prints the line to add). A serial that also belongs to a `role = "primary"` target is refused whatever the mirror entry says: no drive of the 22 TB pair is ever lent.
2. **Only when nothing else has the drive.** It refuses while `das-backup`, `das-backup-full`, `das-scrub` or `das-backup-doctor` is running, while any partition of the drive is mounted on the host, while the VM is running or already has a disk, while an earlier session's holder still runs, and while the VM has a **managed-save image** — the saved memory of a recovery OS that was running when it was saved (`virsh managedsave`, virt-manager's Save, or `libvirt-guests` at a host shutdown). A start would resume that OS where it stopped, its disk included, against a partition 2 the host may have written since; the session never discards it — that is your decision (see the refusal below).
3. **Not an OS whose record says it will run btrbk when it boots.** The drive keeps the host's received backups on the same partition 2 its OS boots from, and that OS has btrbk installed: a btrbk run started by its own boot could delete or rewrite them. The nightly backup run records, for each recovery drive, what booting its OS would run (`/var/lib/das-backup/recovery-os.json`), down to one verdict: `will`, `may` or `no`. All three go on in an attended session, and the session guard (4) is what keeps btrbk from running; a `will` prints the banner `this OS runs btrbk at boot; the guard is stopping it now; disable it in this session` before the boot and again once the guard confirms — disable what the reasons name at the console (`systemctl mask btrbk.timer btrbk.service`, or whatever they name) before you power off (decision 3 of bd `DAS-Backup-Manager-8249`). `--unattended` refuses a `will`, whatever the options; for a `may` the session prints the reasons — what the record could not tell — and masks every unit they name. The record must also be at most 8 days old and made after the drive's last session — the OS may have changed in it; the time of each session is kept in `/var/lib/das-backup/recovery-os-vm-sessions`, written just before the recovery OS is resumed and again when the disk is back. A session that ends before that point — refused at the start, a managed-save image, a start that failed, a domain torn down while still paused — records nothing: that OS never ran, its record still describes it, and the retry is admitted. A boot on bare metal leaves no session time, so after one the record still looks current until the next nightly run checks the drive again: run a backup with the drive attached before the next session. Every session shows the record's verdict, its reasons, the enabled units and its age, also when it goes on. `--accept-boot-record-risk` lets an old record or a session since then through — loudly, and into the summary's Warnings line (exit status 5) — but never a record that is missing, cannot be read, is of another schema, or says nothing about this drive. Nor one that is not of this drive's filesystem: the record names the filesystem it was read from (`mount_uuid`), and a record that names none (written by a btrdasd from before it was kept) or names another than the one `config.toml` mounts the target by is refused, whatever the options — a label pointed at another drive, or a filesystem made again since, never inherits the old drive's verdict. Partition 2 is held to that same `mount_uuid` (1), so a record that passes is one of the disk that is lent.
4. **The session guard: btrbk cannot run while it is booted.** The record is advice, not enforcement: no reading of shell and systemd from the outside is complete, and a stock CachyOS reads `may` for ever. What enforces it exists for one session only. The domain is defined with SMBIOS strings that the recovery OS's systemd, 256 or later, turns into units at boot (`man systemd.system-credentials`):
   - **masks** of `btrbk.service`, `btrbk.timer`, `cronie.service` and `crond.service`, of every cron daemon the record lists as enabled, and of every unit the record names as running btrbk or begins a reason with — plain ASCII unit names only (an instance, never a template, no `:`), never the guard's own, at most 64. On a `will` or `may` record, a named unit it cannot mask refuses the session before anything is taken; on `no` it is a warning (exit status 5);
   - **`das-vm-guard.service`**, which binds a btrbk that refuses — and logs the attempt on the kernel log (`dmesg | grep das-vm-guard`), the evidence that something tried — over every btrbk in `/usr/bin`, `/usr/local/bin`, `/usr/local/sbin`, `/usr/sbin`, `/bin` and `/sbin` (where present), and says so on the console and above every login prompt. It runs after the root is remounted and before udev's coldplug (whose rules can run programs), `local-fs-pre.target` and `sysinit.target` — so before every unit with default dependencies. Not before systemd's own earliest units, anything an OS orders before `systemd-remount-fs.service`, or a unit of its own with `DefaultDependencies=no` that is ordered against none of these;
   - **`das-vm-guard-report.service`**, which checks all of it — the guard active, no masked unit startable, every btrbk present covered — and writes a line through the virtio port `org.dasbackup.guard` at the start of every boot and every minute after, each ending in `seq <n> boot <id>`: its number in this boot and the boot's id (`/proc/sys/kernel/random/boot_id`). It names the masks in full when it starts or its state changes, and by a digest of their list otherwise (`sha256:` and 16 hex digits), so a heartbeat is about 105 bytes whatever the number of masks — about 150 KB a day. Its host side is a file in `/run/das-recovery-os-vm/`, root's only, which virtlogd rotates at its `max_size` (2 MiB by default, so about every 14 days); the session follows the report across that. It is restarted when it fails — killed or crashed, not stopped on purpose (`Restart=on-failure`; at most four starts an hour), and what it has said this boot is kept in `/run/das-vm-guard/report.state`, so a reporter restarted while the guard is lifted goes on with `lifted`, never a first line that says NOT engaged. What it cannot check in one round — systemd not answering during an upgrade's re-exec, `findmnt` or `readlink` failing — it skips, and looks again in 5 seconds.

   Masked here means: for that boot, each unit is replaced by an empty one that systemd refuses to start (systemd 262 lists it as a "bad unit file setting", and the boot log shows those units failing to load: that is the guard), **and** given a drop-in, `zzzzzzzz-das-vm-guard.conf`, whose condition can never hold. A drop-in of the OS's own that gives the unit a command again (`systemctl edit` writes `override.conf`) makes the unit load, but it still never starts: the guard's condition applies after it. Only a drop-in that sorts after the guard's could undo that, and the report then names the unit: the guard is not engaged.

   For a record that names no unit, the first line reads `das-vm-guard engaged 4 masks btrbk.service,btrbk.timer,cronie.service,crond.service`; every unit the record names adds a mask. These drives' records name `mkinitcpio-generate-shutdown-ramfs.service`, so their sessions print `das-vm-guard engaged 5 masks …,mkinitcpio-generate-shutdown-ramfs.service` (observed 2026-10-07). **Every line is judged:** each boot must begin with its line 1, engaged, and no line may say NOT engaged. **A boot must prove itself:** the first boot within 15 minutes of the start, and — since a boot that came without the guard writes nothing at all — every boot after a **reset**. The session watches libvirt's reboot event for the VM (a reboot inside it, or `virsh reset`), and after each one a new boot must report engaged within 15 minutes of the first reset not yet answered; more resets do not push that out. Each reset is read whole even when the session reads it late (its script stopped, say by a Ctrl-Z), and is placed where the report stood when it was read — so a new boot's line written before that cannot answer it, and that boot is asked again: the cautious side. Output of the event stream the session does not recognise but that names a reboot counts as a reset too. A failure on a `will` or `may` record asks the recovery OS to shut down — never forced off — and the session ends with exit status 6; on a `no` record it is a warning (exit status 5), and the session goes on. A recovery OS whose systemd is older than 256 ignores the strings and never reports, which is how that is caught.

   **Silence after a boot proved itself is a warning, never a stop.** The guard's enforcement — the bind mount, the masks — does not depend on its reporter, so 15 minutes without a line and without a reset means its reporter stopped or the recovery OS hung, and the session cannot tell which: it says `THE SESSION GUARD'S REPORTER IS SILENT` (again every 15 minutes, with how to look), and the summary carries it (exit status 5). Two exceptions. If libvirt's event stream ends (`THE RESET WATCH HAS STOPPED`, a libvirt restart), a reset can no longer be ruled out, and from then on 15 minutes of silence fails as above. And neither a host suspend nor a paused VM is silence. Time the VM is paused is not counted, and nothing is said of it. A gap between two of the session's looks far longer than a look takes (`DAS_RECOVERY_VM_CLOCK_GAP_SECS`, 120 s) is not counted either, and the session says so; unless the VM read paused at the looks on both sides of it, the time skipped also goes into the summary's Warnings (exit status 5): the session cannot tell a host suspend from its own script stopped, and in the second the recovery OS ran that long unwatched, every deadline moved out by as much. Lines lost from the report (a gap in a boot's numbers, a file cut in place) are said and put in the summary (exit status 5), never a failure: the boot was seen to begin engaged. So is a line left unfinished when the recovery OS powers off — unless only a NOT engaged line begins so, which fails.

   **What it cannot see:** a new boot that comes *without* a reset — `kexec`, say — and without the guard writes nothing, and is only silence: a warning, not a stop. A new boot that does report is judged like any other (it must begin with its line 1, engaged).

   **It boots only guarded.** The domain is started *paused* and afresh (`--force-boot`: never from a saved image, and a session refuses while one exists, 2 above), its live definition read back, and resumed only if it carries the guard. If it does not (a `define` or `session-end` run at that very moment) it is destroyed while still paused — the one thing this script ever destroys, a domain that has not run a single instruction — and nothing boots. The destroy reads the domain's state and its vCPUs' time in one read right before it (`virsh domstats --state --vcpu`), and refuses anything not paused, or paused with vCPU time — something other than the session resumed it (and paused it again): it may have run, and is treated as an OS without the guard. A read that fails, or lacks a vCPU's time, destroys nothing. libvirt counts a vCPU's time in whole clock ticks (10 ms), so a resume and pause by someone else within that still reads 0. Inside the VM, `systemctl stop das-vm-guard` lifts the bind mount for the update ([below](#the-first-update-at-the-vm-console)); the masks stay until the VM powers off. Afterwards the domain is defined from its template again, which carries no guard; `session-end` does that too for a session that ended before it could. `status` and `session-end` read and judge the guard's report — before anything is removed — and exit 6 (5 on a `no` record) when it says not engaged, is missing for a recovery OS that ran, or cannot be read. They honour the resets the session saw: a reset no boot after it answered is `pending` while the recovery OS is paused, or runs and less than 15 minutes have passed since the reset by the wall clock, time it spent paused included (exit status 0 — treat it as unguarded until it says engaged), and `NOT confirmed` once it is shut off or that time has passed (exit status 6, or 5 on `no`). `status` also counts silence, since it cannot see a reset made after the session's script ended: for a recovery OS that is neither shut off nor paused, a report last written 15 minutes ago or more is `NOT confirmed: silent for …` — the reporter stopped, or a boot came without the guard. That is information only: `status` never stops anything. **On bare metal there is no guard** — see [On bare metal](#on-bare-metal).
5. **The maintenance lock**, `/run/das-maintenance.lock`, taken without waiting and held for the whole session. A backup or scrub that starts meanwhile waits for it; the jobs that would otherwise mount a target defer. Its first line names the session and the process holding it — `recovery-os VM session <label> pid <pid>`, the disk holder's pid once it holds — and a refusal elsewhere prints it; it is emptied again before the lock is let go.
6. **The claim.** `btrdasd recovery-os hold-disk` holds the whole disk open exclusively (`O_EXCL`). While it does, the kernel refuses to mount any partition of the drive on the host — by device or by UUID, from any program — while the VM's own non-exclusive open still works. The holder runs in a session of its own (no Ctrl-C or hangup reaches it) and in a systemd scope of its own (`das-recovery-os-holder-<label>.scope`, outside your login session), and it also holds the lock: a closed terminal, a Ctrl-C or a killed script leaves the drive claimed and the backups waiting for as long as the VM may be using it. A holder that dies while the VM runs is replaced at the next check (the VM's own open does not stand in the way), and the summary says so. A session refuses to start without `systemd-run`. Even so, do not stop your user session manager (`loginctl kill-user`, `systemctl stop user@…`) while a session runs. And never `systemctl stop` a `das-recovery-os-holder-*` scope during a session: that ends the claim at once, and the lock with it unless the session's script still runs (the script holds the lock too) — a running session claims the disk again at its next check (every 5 seconds), one left in place (exit status 3) does not. A lost claim, even one taken again, ends the session with exit status 5, and the summary says whether it was taken again.
7. **SATA, not virtio.** The whole disk is attached as a SATA disk with boot order 1 — the VM's network card has 2, so the network is tried only after the disk. The recovery OS's initramfs almost certainly lacks the virtio drivers. Its default image finds its root on SATA only if it was built with `ahci` — that is, if the OS was installed on a machine with an AHCI controller. If it was installed with the drive already in the USB enclosure, the default image may carry only the USB storage drivers; the first boot in the VM then needs the **fallback** entry (see below). That is the documented path, not a defect.
8. **It waits** until the recovery OS powers itself off — and, from its start and from every reset, at most 15 minutes for a boot's first line (it reports every minute after). Reboots inside the VM are part of the job; the guard comes back at every boot, and each boot's lines are judged.
9. **Giving it back:** detach the disk, define the domain from its template again (no guard), stop the holder, `btrfs device scan` the drive's partition 2 so the host's kernel reads what the other kernel wrote, check that no partition is mounted, take out the egress rule (10) and any console bridge, release the lock, print a summary.
10. **The VM's traffic goes direct.** Through libvirt's NAT the VM follows the host's default route — through a VPN exit node when one is up, where a full update crawled at about 0.1 MB/s on 2026-10-07. Before the VM starts, the session puts a policy rule in place for the VM network's subnet only, read from `virsh net-dumpxml default` — `ip rule add from 192.168.122.0/24 lookup main priority 5100`, above Tailscale's `lookup 52` at 5270 — and takes it out when the disk is given back. A session left in place (exit status 3) leaves it; `session-end` takes it out once no recovery OS runs. One found already in place (a session that did not finish) is taken over and taken out. A rule that cannot be put in place refuses the session before anything boots; one that cannot be taken out is said, exit status 5. `status` says whether one is noted and in place.

The host never writes to the drive: no mount, no chroot, no copy. The script never forces off a recovery OS that has run — it could be in the middle of an update: it asks it to shut down (ACPI), and asks again every 20 seconds for up to 10 minutes, since a request sent while it is in its firmware, boot menu or initramfs is dropped. Interrupted, or out of time, it leaves the VM, the claim and the lock in place and says how to finish (exit status 3) — and says first what the guard was, judging what reached the report since its last look: engaged, with how long ago it last reported (from then on nothing watches it but `status`, which counts silence too); a boot after a reset that has not reported yet (`THE BOOT AFTER THE RESET HAS NOT BEEN JUDGED`: `status` first, and treat it as unguarded until it says engaged); not confirmed, in which case it tells you to power the recovery OS off now, never to let an update finish; or not yet judged, with where its report is and `status` to run first. If it was asked to shut down and has not gone, it says how often and over how long, and what forcing it off trades: a recovery OS without a confirmed guard running beside the backups, against an update in it cut short. `virsh destroy` is then **your** choice, on no clock; the script never makes it. A VM left paused after a resume was tried runs nothing — not even a power-off from its console: resume it first, or destroy it as your choice. Interrupted just as the recovery OS powered off, it judges the report before giving the disk back, and exits 6 when the guard did not confirm on a `will` or `may` record. A session that ended but needs a look — the claim was lost while the VM ran, the drive re-enumerated during it, a partition was mounted afterwards, the device scan failed, the boot-record check was overridden, the guard did not confirm on a `no` record, its reporter went silent, lines of its report were lost or a last one left unfinished, time was skipped between two looks (a host suspend, or the script stopped), the reset watch stopped, or the guard could not be taken out of the definition again — exits 5 and says why in the summary; a dry run that needed `--accept-boot-record-risk` exits 5 too. A guard that did not confirm on a `will` or `may` record ends the session with exit status 6, once the recovery OS is off and the disk is back.

#### Running it

As root. The script is in `/usr/lib/das-backup/` on a `/usr` install, `${prefix}/lib/das-backup/` otherwise; it needs libvirt with KVM, the UEFI firmware (`edk2-ovmf`), libvirt's NAT network `default`, `systemd-run`, `jq` (a session refuses without it), `ip` (iproute2, for the egress rule), for `screenshot` ImageMagick's `magick`, and for `console-socket` `socat` — see [INSTALL.md](INSTALL.md#recovery-os-vm-optional). Once after installing, and again whenever an upgrade of this project changes the VM's definition (every drive's VM must be shut off, with no disk). The first `define` after upgrading from a release with one shared VM retires that VM:

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
virt-viewer --connect qemu:///system --attach recovery-os-updater-system-recovery-A-2tb
```

Other commands:

- `session A --timeout 120` — after 120 minutes, ask the recovery OS to shut down and wait 10 more; it is never forced off. If it is still running then, the session is left in place (exit status 3).
- `session A --accept-boot-record-risk` — only when you know why the boot record says what it says (3 above): the session goes on despite an old record, or a session since the record was made, and its summary says so. The guard (4) still applies, and a guard that does not confirm still shuts the recovery OS down.
- `status` — each drive's VM: its state, the disk it holds, the session guard and its report's judgement (`engaged` with its last report's age, `no report yet`, `never ran`, `pending: the recovery OS was reset …`, `NOT engaged: …`, `NOT confirmed: the recovery OS was reset …`, `NOT confirmed: silent for …`, `cannot be judged: …` when its session state is gone — a host restart clears `/run` — or, with the guard in the definition and no session state at all, `cannot be judged: no session state for it`; exit status 6, or 5 on a `no` record, for the three that begin with NOT, and 6 for the two that cannot be judged), the holder and its scope, who holds the maintenance lock, and whether an egress rule is noted and how many are in place (`unknown`, with ip's words, when `ip rule show` fails — never 0). A shared `recovery-os-updater` still defined is named, with `define` to retire it. A definition virsh cannot read makes the guard `unknown (virsh: …)`, exit status 6. A VM that is not defined at all has no guard (exit status 0).
- `screenshot A screen.png` — drive A's VM screen, while it runs (read through QEMU's monitor, so it does not need the VNC socket).
- `session A --unattended`, `session A B [--mode sequential|parallel]`, `history`, `clean-runs A`, `console-socket A <uid>` — [below](#unattended-updates).
- `session-end A` — finishes a session whose script was interrupted or killed (exit status 3 or 4), once the recovery OS is powered off: judges the guard's report first and says what it found (exit status 6, or 5 on a `no` record, when it was not engaged, is missing for a recovery OS that ran, cannot be read, or a reset the session saw was never answered by a boot reporting engaged; 5 when its last line was left unfinished), detaches, defines the VM from its template again (no guard), stops the holder (which releases the lock), rescans. With no disk attached it still takes out a guard left in the definition. Whenever it changes the VM's definition with no live holder, it takes the maintenance lock first, and refuses while another job holds it.

#### The first update, at the VM console

The first time is by hand at the console — it needs your login on the recovery OS. Step 2 also prepares later sessions: virtio drivers let the default boot entry run in a VM on any controller, and the guest agent (step 3) allows the update to be automated.

0. **Look before changing anything.** This OS has not run for months, and it now runs beside the host's backups. Its `RECOVERY OS` block in the last backup report is what the session went on (see [Keeping the recovery OSes current](#keeping-the-recovery-oses-current)); here, check what it does not show:

   ```bash
   systemctl list-timers --all            # what will fire while it runs
   ls /etc/pacman.d/hooks/                # an esp-mirror hook here fails harmlessly in the VM (no NVMe ESPs)
   grep -l btrbk /etc/pacman.d/hooks/* /usr/share/libalpm/hooks/* 2>/dev/null   # a hook that runs btrbk
   grep -v '^#' /etc/fstab                # a line for something only the workstation has needs nofail
   pacman -Q das-backup-manager 2>/dev/null && systemctl list-unit-files 'das-*'
   ```

   A pacman hook that names btrbk (a snapshot before every upgrade, say) runs during the very pacman commands below, after the guard is lifted, when the real btrbk can run: move it out of the hook directory, inside the recovery OS, before step 1. If das-backup-manager is installed here, its timers must not fire in the recovery OS — it is not the host, and its jobs would act on whatever disks it sees. Disable every one that is enabled, inside the recovery OS (an install of another version may have a different set):

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

1. **Lift the session guard, then the keyrings first.** The console and the login prompt say the guard is on: a btrbk that refuses is bound over `/usr/bin/btrbk` (4 above). pacman cannot replace a file something is mounted on — and does not fail for it: on a test VM it logged `cannot remove /usr/bin/btrbk (Device or resource busy)`, exited 0, and recorded btrbk as reinstalled, while the file on disk stayed the old one. A package database that says one thing and a disk that holds another is the silent failure this step prevents. So before any pacman, lift it:

   ```bash
   sudo systemctl stop das-vm-guard      # unbinds btrbk; its units and cron stay masked until the VM powers off
   ```

   If `systemctl status das-vm-guard` says **failed** (one bind worked, the next did not), `stop` does nothing — a unit that failed has no stop to run — so unbind by hand, each place it binds (one that is not mounted only says so), then check that nothing is left:

   ```bash
   sudo umount /usr/bin/btrbk /usr/local/bin/btrbk /usr/local/sbin/btrbk /usr/sbin/btrbk /bin/btrbk /sbin/btrbk
   findmnt -rn -o TARGET | grep -E '/btrbk$'   # prints nothing
   ```

   **From here until you engage it again (step 3) the real btrbk can run** — started by hand, by a pacman hook (step 0 moved any aside), or by a unit the boot record did not name: the masks cover only btrbk's own units, cron and what the record names. Then the keyrings, on their own — an install left alone for months cannot verify today's packages without them. If the mirrors it knows are stale, refresh the mirror list first (`sudo pacman -Sy cachyos-mirrorlist` or, when installed, `rate-mirrors`):

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

   Then check that btrbk itself was replaced — pacman's exit status cannot tell you (step 1). Check with the guard lifted, as it is now: with it on, `-Qkk` reads the refusing btrbk through the mount and reports btrbk altered whatever is on the disk.

   ```bash
   pacman -Qkk btrbk                     # must end with: 0 altered files
   ```

   Any altered file means the upgrade ran with the guard still on. Lift it and reinstall btrbk, then check again: `sudo systemctl stop das-vm-guard && sudo pacman -S btrbk && pacman -Qkk btrbk`. Once it reads 0 altered, **engage the guard again** — nothing more needs btrbk replaced:

   ```bash
   sudo systemctl start das-vm-guard
   ```

4. **Reboot inside the VM** (`sudo reboot`), log in again, and check that `uname -r` shows the new kernel — the version `pacman -Q linux-cachyos` reports. The guard is back after the reboot (every boot of the session reads it again): lift it again before any further pacman or `pacman -Qkk`. Then `sudo poweroff`: the session gives the disk back and prints its summary.

5. **Then boot the drive once on bare metal**, from the enclosure, and confirm its **default** entry boots — the drivers pinned in step 2 are what keep it booting there. Only then count the drive as updated, and remove the rollback point (step 0).

If something goes wrong at the console:

- **The default entry does not find its root** (an emergency shell, or "waiting for root device"): reboot, and at the boot menu pick the **fallback initramfs** entry — it is built without autodetection and carries every storage driver. Expected on the first VM boot of an OS installed with the drive already in the enclosure; step 2 fixes the default entry for next time. The session gives the recovery OS **15 minutes from its start** for the guard's first report, and as long from the first reset not yet answered — a systemd-based initramfs waits 90 seconds for the root before its emergency shell, and the detour through the fallback entry fits easily — but do not leave it at the menu or that shell: past 15 minutes a `will` or `may` session asks it to shut down (exit status 6, or 3 if it does not go, since an emergency shell may ignore the request).
- **The firmware finds no bootloader** ("No bootable option or device was found", or it tries a network (PXE) boot — the network card is second in the boot order — and then lands in its UEFI shell): the VM's firmware starts with no boot entries of its own and looks for `EFI\BOOT\BOOTX64.EFI` on the drive's ESP. Restart the VM — `exit` at the UEFI shell, virt-viewer's Send key → Ctrl+Alt+Del, or `sudo virsh reset recovery-os-updater-<label>`, which is harmless while only the firmware runs — press **Esc** during the three-second splash, choose **Boot Maintenance Manager → Boot From File**, pick the drive's ESP and its loader (`EFI/systemd/systemd-bootx64.efi` for systemd-boot). Once booted, `sudo bootctl install` puts systemd-boot at the fallback path as well — the recovery OS writing its own ESP, from inside.
- **An emergency shell over a missing mount**: an `/etc/fstab` line for something only the workstation has, without `nofail`. Add `nofail` to it, from the recovery OS itself.
- **`virsh attach-device` fails with "per-device boot elements cannot be used together with os/boot elements"**: the VM was defined from an older definition, without per-device boot, and libvirt filled in a boot device of its own (`<boot dev='hd'/>` under `<os>`), which refuses the disk's. The boot order is per device: the drive 1, set when a session attaches it, and the network card 2, in the definition. With the VM shut off and no disk attached, run `define` once more, then the session again — the failed attach gave everything back.
- **The session ends with exit status 6: `the session guard did not confirm`** — the recovery OS was asked to shut down (again every 20 seconds), and the summary says why. **If it does not go within 10 minutes**, the session is kept instead (exit status 3) and says `THE RECOVERY OS IS RUNNING WITHOUT A CONFIRMED SESSION GUARD`, how often it was asked and over how long. Power it off from its console now (`systemctl poweroff`). If it does not go, forcing it off (`sudo virsh --connect qemu:///system destroy recovery-os-updater-<label>`) is your choice, and the session says what it trades: a recovery OS without a confirmed guard running on beside the backups, against an update in it cut short, whatever it was writing half written. Then `session-end`, which judges the report again. The reasons:
  - `no report from the recovery OS within 0h 15m 00s`: either its systemd is older than 256 — the version that turns SMBIOS strings into units, released in June 2024, so not an install from 2025 or later — and the guard never existed in it; or it never reached `sysinit.target` (an emergency shell, e.g. over a missing mount: see above), or it took longer than that to get there. Do not boot it on bare metal to find out: there is no guard there either ([On bare metal](#on-bare-metal) says when that is safe). An OS too old for the guard is updated on bare metal, and only when its record says `no`.
  - `the recovery OS reports it NOT engaged: …`: the guard ran and found something wrong — a unit not masked (a drop-in of the OS's own sorting after the guard's `zzzzzzzz-das-vm-guard.conf`, say), `das-vm-guard.service` not active or failed, or a btrbk the refusing one does not cover. Its journal says why: `journalctl -b -1 -u das-vm-guard -u das-vm-guard-report` at its next boot.
  - `boot <id> began without the guard engaged`: the recovery OS rebooted, and the new boot's first report was not the engaged line. `boot <id>: its first report was not seen`: a new boot's line 1 never arrived, so whether it began engaged cannot be known.
  - `the recovery OS was reset, and no boot after that reported its guard engaged within 0h 15m 00s`: it rebooted and came back without the guard (no units from the SMBIOS strings at all — a boot that says nothing), or it is stuck in its firmware, boot menu or an emergency shell.
  - `the recovery OS stopped reporting … with the reset watch down`: libvirt's event stream ended during the session, so a reset could no longer be seen, and from then on 15 minutes of silence counts as a boot without the guard.
- **`THE SESSION GUARD'S REPORTER IS SILENT`** (a warning, said again every 15 minutes; the summary's Warnings line, exit status 5): no line for 15 minutes, and no reset since its last one. The guard itself does not depend on its reporter, so nothing is stopped for it: either the reporter died for good (it is restarted when it fails, at most four starts an hour, and never after a `systemctl stop`) or the recovery OS hung. Look at its console (`virt-viewer --connect qemu:///system --attach recovery-os-updater-<label>`; inside it: `systemctl status das-vm-guard-report das-vm-guard`). The one case this cannot catch: a new boot *without* a reset (`kexec`) that came without the guard — it too is only silence.
- **The session refuses: `the started domain does not carry the session guard`** (or `cannot read the started domain's definition`): something defined the VM again between the session's checks and its start — a `define` or `session-end` run at that very moment. The domain was destroyed while still paused, before it ran a single instruction — no firmware, no OS, no write to the disk; nothing booted, and the session says so. A retry is admitted: the session records its time only as it resumes the recovery OS, and this one never ran — run the session again. **If it fails the same way again**, the cause is persistent — libvirt not applying the session guard's SMBIOS strings to the started domain, for one — and needs a fix before any session can boot. If it adds `and it could not be destroyed`, the session is kept instead (exit status 3) and says the domain `was started paused and never resumed`: it has still run nothing. First check it is still paused and has never run — `sudo virsh --connect qemu:///system domstats --state --vcpu recovery-os-updater-<label>` must say `state.state=3` and `vcpu.N.time=0` for every vCPU; anything else means something other than the session resumed it, and it may be running (or have run) without the guard (`status`) — and only then tear it down: `sudo virsh --connect qemu:///system destroy recovery-os-updater-<label>`. Then `session-end`, which reports the guard as `never ran`. If the session says `not destroying recovery-os-updater-<label>: it is running, not paused`, something resumed it between the start and the check: it is treated as an OS without the guard (asked to shut down on a `will` or `may` record). `it is paused, but its vCPUs have run`: something resumed it and paused it again; it is kept (exit status 3) as an OS that may have run — resume it and judge it, or destroy it as your choice. `whether it ever ran cannot be read`: nothing is destroyed, and it is kept (exit status 3) with the check above.
- **The session refuses: `has a managed-save image`**: the VM holds the saved memory of a recovery OS that was running when it was saved, and a start would resume it — its disk included — instead of booting it afresh. The session never discards it. If you decide that state can go (whatever that OS had not yet written to its disk is lost): `sudo virsh --connect qemu:///system managedsave-remove recovery-os-updater-<label>`, then run the session again (nothing was recorded for the refused session, so the retry is admitted). **Never let anything save this VM:** do not enable `libvirt-guests.service` (its default here is `ON_SHUTDOWN=suspend`) for it, and keep virtqemud's `auto_shutdown_try_save` off — a host shutdown during a session would otherwise save the recovery OS with its disk, and the next host boot could restore it with no claim and no lock while backups mount the same partition.
- **The session refuses: `names units the session guard cannot mask`**: the boot record says btrbk may (or will) run and names a unit the guard cannot put into a credential — a template (`name@.service`), a name with `:` or a non-ASCII character, a target — or more than 64 of them. Put it right in the recovery OS on bare metal, as for a record that says `btrbk will run when this OS boots` below, then let a backup run record it again.

  Before you boot it again, look in that boot's journal for anything that ran btrbk (`journalctl -b -1 | grep -i btrbk`, at its next boot), and let the next backup run record the drive again. On a `no` record the same causes are a warning instead, and the session goes on.
- **`pacman -Qkk btrbk` reports altered files after the update**: the upgrade ran with the guard on, and pacman could not replace btrbk (step 1 of the first update). Lift it, reinstall, check: `sudo systemctl stop das-vm-guard && sudo pacman -S btrbk && pacman -Qkk btrbk`. If it still reports altered files, the guard had **failed** and `stop` did nothing: unbind by hand — `sudo umount /usr/bin/btrbk /usr/local/bin/btrbk /usr/local/sbin/btrbk /usr/sbin/btrbk /bin/btrbk /sbin/btrbk`, then `findmnt -rn -o TARGET | grep -E '/btrbk$'` prints nothing — and reinstall again.
- **A record says `btrbk will run when this OS boots`**: booting it as it is would start btrbk against the backups on the same partition. In the VM, an attended session boots it anyway under the guard, with the banner above, and you disable the runner at its console; an unattended one refuses (`a "will" drive is updated attended only`). **On bare metal** nothing stops it: fix it from its **emergency** shell, and never let that boot go on. Not its rescue shell: `rescue.target` still starts all of `sysinit.target` — every early service, every `/etc/fstab` mount, and whatever an install enabled under `local-fs.target.wants` or a udev rule — and a runner enabled under exactly such a wants is when the record says `will`. `emergency.target` pulls in nothing but the shell (systemd.special(7): it "does not pull in other services or mounts"; `sysinit.target` conflicts with it).
  1. At its boot menu, edit the default entry's kernel line (systemd-boot: `e`) and add `systemd.unit=emergency.target systemd.setenv=SYSTEMD_SULOGIN_FORCE=1`. The second word gives the root shell even when the root account is locked: `systemd.setenv=` puts the variable in the environment of the emergency shell's unit, which every systemd version passes on; a bare `SYSTEMD_SULOGIN_FORCE=1` on the kernel line is read directly only by newer ones (254 and later, by the review of this guide; this workstation's 262 does).
  2. If `/` is read-only there: `mount -o remount,rw /`.
  3. Mask or disable the runner the record's reasons name — `systemctl mask btrbk.timer btrbk.service`, or remove its link from the `.wants` directory the record names — or move its btrbk config away.
  4. `systemctl poweroff`.

  **Never `exit` or Ctrl-D** at that shell: leaving it goes on to the normal boot, timers included. If you are asked for a password instead ("… root password … or press Control-D to continue"), Ctrl-D boots it **normally**: type the root password, or power off. To power off at any prompt, **hold the power button** (in the VM: the console's force-off) — **never Ctrl-Alt-Del**, which is `reboot.target`, and the next boot takes the default entry.

  Not verified for these drives: the recovery OS's own systemd version, whether its root account is locked, and its systemd-boot `editor` setting (`loader/loader.conf`). With `editor no` the kernel line cannot be edited at the menu: then boot an entry that goes to emergency mode by itself, if the menu has one — the "fallback initramfs" entry is not one, it boots normally — and otherwise do not boot it; ask. The host does not write the recovery OS to fix it. This is advice; the script does none of it.

  Then let the next backup run, with the drive attached, record it again.
- **The session refuses: `cannot tell when this drive's last VM session ended`**: the session-times file, `/var/lib/das-backup/recovery-os-vm-sessions`, is empty or has a line that is not `<label> <seconds>` (a hand edit, or a crash during a write). A new boot record does not help here: the file itself has to be put right. Remove the bad line, or the whole file, then run a backup with the drive attached, so that the record is newer than any session you are unsure of. (A session forced through with `--accept-boot-record-risk` rewrites the file from its well-formed lines and says what it did: another drive's line with no readable time comes back as that drive's line at the session's time, anything else is dropped.)
- **The session refuses: the record is older than 8 days, or was made before this drive's last VM session ended**: let a backup run, with the drive attached, record it again.
- **The session refuses: `no boot record`, `cannot be read`, `is not one JSON document`, `is not one this script can read` (a schema that is not a number), `is schema N -- this script reads schemas 3 and 4 only`, `has no entry`, `has no check time`, `has a check time this host cannot read as a date`, `is dated more than a day in the future`, `has no inspected OS` or `has no btrbk-at-boot verdict`**: the same — the record comes from a backup run that checked this drive, and no flag stands in for it.
- **The session refuses: `does not name the filesystem it was read from`, or `was read from filesystem X, not Y`**: the record was written by a btrdasd from before it named the filesystem, or its filesystem could not be told, or it is of another filesystem than the one `config.toml` now mounts the target by — the label was pointed at another drive, or the drive's filesystem was made again. The same again: let a backup run, with the drive attached, record it; no flag stands in for it. (If `config.toml` is what is wrong, put its `mount_uuid` right first: `sudo btrdasd setup --check` prints the line.)
- **The session refuses: `Unit das-recovery-os-holder-<label>.scope already exists`**: the scope of an earlier holder is still loaded, usually failed. If `status` shows no session running, clear it with `sudo systemctl reset-failed das-recovery-os-holder-<label>.scope` and run the session again.
- **`virsh start failed: … network 'default' is not active`**: `sudo virsh net-start default && sudo virsh net-autostart default`. The failed start gave everything back, and recorded no session time (the recovery OS never ran): run the session again.

#### Unattended updates

Once a drive's OS runs the QEMU guest agent, its update can run without you at the console: the session drives it through the agent (`virsh qemu-agent-command`, `guest-exec` — no SSH, no password, never a bypass of its login: you installed the agent yourself, in an attended session).

**Before the first one**, in an attended session of that drive: `pacman -S qemu-guest-agent`. On Arch and CachyOS nothing more is needed — the package's udev rule starts it when the VM's agent port appears (`systemctl is-enabled qemu-guest-agent` says `static`, which is right). The next backup run records it: the drive's entry in `/var/lib/das-backup/recovery-os.json` (record schema 4) carries `guest_agent` — `installed`, `enabled` (started at boot: its unit enabled, or the package's udev rule there, and nothing in `/etc` masking or replacing either) and `why`. An unattended session is refused unless that says installed and enabled, and on a `will` record whatever the options — a `will` drive is updated attended, where you disable what runs btrbk; a record of schema 3 (from a btrdasd before this one) has no agent reading and is refused for `--unattended` until the next backup run records the drive again.

```bash
sudo /usr/lib/das-backup/recovery-os-vm.sh session A --unattended
```

Everything an attended session does still applies — the checks, the lock, the claim, the guard and its judgement, the egress rule, giving the disk back. In between, these steps run in the recovery OS, each as a transient systemd unit of its own (`das-vm-update-<n>-<step>`, its script, output and exit status in `/run/das-vm-update/`), so pacman upgrading the agent itself, or systemd re-executing, does not end it; the session looks at each one's exit status and new output every 5 seconds and prints the output as it comes:

1. **boot** — wait until the session guard reports engaged (as always, within 15 minutes) and the agent answers (`DAS_RECOVERY_VM_AGENT_SECS`, by default as long).
2. **egress** — the VM's public address (api.ipify.org) and its owner (ipinfo.io) must be the direct path's: `AS209` by default, the author's ISP; set `DAS_RECOVERY_VM_EGRESS_ORG` for yours. Anything else — a VPN exit node — stops the update before anything is changed.
3. **snapshot** — the recovery OS snapshots its own `@`, read-only, to `@.pre-update.<YYYYMMDD-HHMM>` at its filesystem's top level, and deletes all but the newest two such snapshots. Its own btrfs, never the host's: the way back from a bad update, taken by the drive's own OS. Rolling back is done from inside that OS too (or a live system booted on bare metal), never from the host.
4. **guard-lift** — `systemctl stop das-vm-guard`: pacman cannot replace a file something is mounted on. The masks hold.
5. **keyrings** — `pacman -Sy --noconfirm --needed archlinux-keyring cachyos-keyring`.
6. **upgrade** — `pacman -Syu --noconfirm`, at most 2 hours (`DAS_RECOVERY_VM_UPDATE_SECS`).
7. **packages** — `pacman -S --needed --noconfirm qemu-guest-agent`, then `pacman -S --noconfirm amd-ucode intel-ucode`, reinstalled whatever is installed: on 2026-10-07 drive B had both packages and neither image on its ESP, which every loader entry names.
8. **initramfs** — every preset in `/etc/mkinitcpio.d/` must build the fallback image its Fallback entry boots: one with only `PRESETS=('default')` (drive A's, on 2026-10-07) gets `'fallback'`, `fallback_image` (its `default_image` with `-fallback`) and `fallback_options="-S autodetect"`; one of any other shape without it stops the update. Then `mkinitcpio -P`.
9. **verify-btrbk** — `pacman -Qkk btrbk` must report `0 altered files`.
10. **verify-boot** — inside the recovery OS: every `linux`, `initrd` and `efi` file each `loader/entries/*.conf` names must be on its ESP, and every `LABEL=`, `UUID=`, `PARTUUID=` and `PARTLABEL=` in `/etc/fstab` must resolve to a device — drive B's named its ESP by the label of before the relabel, which stopped its boot in emergency mode (bd `DAS-Backup-Manager-ac82`).
11. **guard-engage** — `systemctl start das-vm-guard`, and every btrbk present is bound again.
12. **reboot** — a new boot (another boot id) whose guard reports engaged, answering the reset, and whose agent answers.
13. **kernel** — `uname -r`, into the summary and the history.
14. **poweroff** — through the agent, and by ACPI until it is off.

**Anything unexpected stops it**: a step that exits nonzero, takes longer than its limit (each has one; it may still run in the recovery OS then), cannot be started, or meets an agent silent for 5 minutes (`DAS_RECOVERY_VM_AGENT_GRACE_SECS`); the guard not confirming; the recovery OS powering off by itself; `--timeout` (here: for the whole update). The recovery OS is asked to power off — through its agent, and by ACPI again every 20 seconds, never forced — the disk is given back, and the session exits **7**, the summary's `Unattended` line and the `PROGRESS <label> <step> fail <why>` line naming the step and why; nothing after it ran. Not off within 10 minutes, the session is kept (exit status 3), as an attended one would be. The guard's own failures keep their status: 6 on a `will` or `may` record.

**Stopped at keyrings, upgrade, packages or initramfs, the recovery OS may be half-upgraded**: the power-off can end pacman or mkinitcpio in the middle of its work (a step past its limit, or `--timeout`, is still running when it comes). The summary then says so on a line of its own (`Unattended    WARNING: the recovery OS may be half-upgraded …`) and names the snapshot step 3 took, `@.pre-update.<YYYYMMDD-HHMM>`: roll back to it from inside that OS or a live system before trusting the drive.

#### Both drives in one run

```bash
sudo /usr/lib/das-backup/recovery-os-vm.sh session A B --unattended                    # one after the other
sudo /usr/lib/das-backup/recovery-os-vm.sh session A B --unattended --mode parallel    # both at once
```

One process holds the maintenance lock, once, for the whole run — backups wait for the run, not for each drive — and the egress rule, and runs each drive's session with that lock (each drive's lines begin `[<label>]`; its end is a `DRIVE <label> <exit>` line). **Sequential** (the default) runs the second drive only after a first that exited 0: a bad update must not reach both, so a known-good recovery OS remains. A first drive's exit 5 goes on to the second only when every cause stayed inside that drive's own session — the guard unconfirmed on a `no` record, the reporter's lost lines, the guard left in the domain's definition, the egress rule not taken out, a dry run with `--accept-boot-record-risk`; a cause in the host, the enclosure or the mechanism (the claim lost, the drive re-enumerated, a partition mounted afterwards, a failed device scan, skipped time, the reset watch stopped), no cause recorded, or one the run does not know stops it, since it may meet the next drive too. Either way the warning and the run's closing lines name the cause. **Parallel** runs both at once, halving the time backups wait, and loses that: both drives take the same update. Each drive still keeps its `@.pre-update` snapshots. The run exits with the gravest of its drives' statuses — 4, 3, 6, 7, 5, then 1 (a drive skipped is not one) — and 5 when its egress rule could not be taken out. A drive left in place (3 or 4) keeps the lock through its own holder, and the rule stays for `session-end`. Without `--unattended` both VMs boot for you at their consoles.

#### The session history

Every session that took the lock adds one JSON line, when it ends, to `/var/lib/das-backup/recovery-os-vm-history.jsonl` (beside the boot record, readable by all): `label`, `start` and `end` (seconds since the epoch), `mode` (`single`, `sequential`, `parallel`), `unattended`, `outcome` (`clean` for exit status 0, `warnings` for 5, `kept` for 3 or 4, `failed` for anything else), `exit`, `overridden` (`--accept-boot-record-risk` let something through), `kernel` (an unattended session's, after its reboot) and `stopped_at` (the step an unattended update stopped at). A dry run, or a session refused before it took the lock, adds nothing.

```bash
/usr/lib/das-backup/recovery-os-vm.sh history A        # drive A's sessions
/usr/lib/das-backup/recovery-os-vm.sh clean-runs A     # its consecutive clean unattended sessions
```

`clean-runs` counts from the newest end back: a session that failed, was kept, ended with warnings or was overridden sets it to 0; a clean attended one neither counts nor resets. A history line that cannot be read makes it refuse (exit status 1) rather than guess. The GUI offers both drives in parallel by default once each drive has 3.

#### The console through a private socket

For a desktop program (the GUI's helper) to show a running session's console without the libvirt group or any change to libvirt's access control:

```bash
sudo /usr/lib/das-backup/recovery-os-vm.sh console-socket A 1000    # prints the socket's path
```

It offers the VM's VNC, which libvirt keeps on a socket only root can open, through a socket of its own: in a fresh directory `/run/das-recovery-os-vm/console-<label>-<random>/` (mode 0711, root's: passed through, not listed, and only root writes in it — a directory of the user's would let them swap a symlink in where `socat` applies the socket's owner by path), the socket `vnc.sock` (mode 0600, the user's), proxied by `socat` for **one** connection — a viewer that disconnects needs a new one (run it again; the new one replaces the old). Only while a session of that drive runs; taken away when its disk is given back (`session-end` too). `/run/das-recovery-os-vm` is `0711`, so the user can reach that one directory and list nothing.

#### Scheduled sessions

The helper's `RecoveryOsScheduleSet` (the GUI's panel; polkit action `org.dasbackup.recovery-os`) arms one unattended session for a time at least 2 minutes ahead, as a generated pair in `/etc/systemd/system`: `das-recovery-os-update-<label>.{service,timer}` for one drive, `das-recovery-os-update-both.{service,timer}` for the two (optionally `--mode parallel`). The service is a oneshot that runs `recovery-os-vm.sh session <labels> --unattended --wait-lock 180`; the timer is a single `OnCalendar=` in the host's local time with `Persistent=false`.

- **`--wait-lock 180`** (minutes: 3 hours) makes the script wait for the `das-backup`, `das-backup-full`, `das-scrub` and `das-backup-doctor` units to be inactive and for `/run/das-maintenance.lock` to be free, looking only; the lock is still taken without waiting, so a job that slips in between is refused, never raced. After 3 hours it refuses (exit 1, in the service's journal), having held nothing. This, not the helper, is what keeps a schedule from colliding with a backup that overran or the other drive's schedule.
- **`Persistent=false`**: a time that passed while the host was off is not run at the next boot. The status document reads such a schedule as **`missed`** (the time passed, or the timer was stopped, without a trigger); the other states are `pending` (the timer has a next elapse), `running` and `fired` (with the newest history line's outcome, or `refused:` and the service's last journal lines when the session never took the lock).
- **The service carries `SuccessExitStatus=1 3 4 5 6 7`**, so a session that refused, warned or failed is not a failed unit (cachyos-sentinel restarts failed units); the outcome is in the session history ([above](#the-session-history)), never in a failed unit.
- **`SessionEnd` is refused** while a session job or a scheduled unit runs, naming it.
- **A cancel** (`JobCancel` on the session job) sends the script one SIGINT, to its process group, as Ctrl-C would. **In `--mode parallel` that does not yet stop the drive sessions themselves** (script defect, bd `DAS-Backup-Manager-c8lf`): a cancelled parallel run goes on to its own end.
- **A masked (symlinked) unit is refused**, on set and on clear: "unmask it or clear the schedule first". The helper never writes through a link.
- **Uninstall** (`btrdasd setup --uninstall`) stops and removes every `das-recovery-os-update-*` unit.

To clear a schedule by hand (what setting the time to 0 does):

```bash
systemctl disable --now das-recovery-os-update-<label>.timer && rm /etc/systemd/system/das-recovery-os-update-<label>.{timer,service} && systemctl daemon-reload
```

#### Progress lines

Every session prints, beside its lines for people, lines for programs: `PROGRESS <label> <step> <start|ok|fail> [message]` (steps: `preflight`, `start`, `wait` for an attended session's wait, `boot`, the unattended steps above, `giveback`), `OUTPUT <label> <step> <text>` (what a step printed in the recovery OS), `RESULT <label> <exit> <outcome>` (a session that took the lock ended; outcome as in the history) and, for two drives, `DRIVE <label> <exit|skipped>`. The script's header holds the same grammar.

### On bare metal

**There is no session guard here.** It exists only in the VM, as SMBIOS strings of the VM's definition — a machine's own firmware carries none of them — so whatever the drive's OS has enabled runs, btrbk included, against the backups on its own partition 2. Before booting it, read its `RECOVERY OS` block in the last backup report (or `btrdasd health`), or the record itself:

```bash
sudo jq '.drives["system-recovery-A-2tb"] | {checked: (.checked_epoch | todate), mount_uuid, btrbk_at_boot: .os.btrbk_at_boot}' /var/lib/das-backup/recovery-os.json
grep -A8 'label = "system-recovery-A-2tb"' /etc/das-backup/config.toml | grep mount_uuid
```

`no`, a recent check time, and the record's `mount_uuid` the one `config.toml` gives the target: go on. `will` or `may`: in the VM, the guard keeps btrbk from running (disable what the reasons name in the session); on bare metal, boot it to its emergency shell first and disable `btrbk.timer` and whatever else the reasons name — see "A record says `btrbk will run when this OS boots`" above. A `mount_uuid` that is null or another: the record is not of this drive's filesystem; let a backup run with the drive attached record it again before trusting it.

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

On the system that runs the backups, check before reconnecting or power-cycling the enclosure that nothing holds it: `sudo flock -n /run/das-maintenance.lock true || echo WAIT` — on `WAIT` a backup, scrub or recovery-OS VM session is using it; leave it alone until that ends. The backup targets never appear in a file manager or under `/run/media`: they are hidden from udisks on purpose. Look for them with `lsblk -o NAME,SERIAL,LABEL,UUID` and mount them by UUID. After reconnecting the enclosure, run `sudo btrfs device scan` so the two-drive primary is registered before mounting it.

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

1. **BTRFS documentation**: https://btrfs.readthedocs.io
2. **Arch Wiki (BTRFS)**: https://wiki.archlinux.org/title/Btrfs
3. **btrbk Documentation**: https://github.com/digint/btrbk
4. **Your distro's support forum** -- for distro-specific recovery steps

---

*Backup system version: 0.7.23.0*
