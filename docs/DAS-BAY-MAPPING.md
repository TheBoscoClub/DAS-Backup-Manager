# DAS Bay Mapping Guide

Bay mapping documents which physical bay in your DAS enclosure holds which drive. This is essential for:

- **Identifying drives during failure** -- LED activity tells you which bay has the failing drive
- **Matching serials to config** -- your `config.toml` target entries reference drives by serial number
- **Safe hot-swap** -- knowing which bay to pull without disrupting the wrong drive
- **Recovery procedures** -- disaster recovery steps reference bays and serials

## Why Device Letters Are Unreliable

Linux assigns device letters (`/dev/sda`, `/dev/sdb`, etc.) based on detection order, which changes on every reboot, USB reconnect, or hotplug event. **Never** reference DAS drives by device letter in persistent configurations. Use serial numbers instead.

## How to Map Your Bays

### Step 1: Identify drives by I/O activity

Generate sustained I/O on one drive at a time and watch which bay's LED blinks. Address each drive by its serial-bearing `by-id` name, so the serial you record is the one that blinked:

```bash
# List the DAS drives with their serials
lsblk -d -o NAME,SIZE,MODEL,SERIAL,TRAN
ls /dev/disk/by-id/ | grep -v -- -part

# Read from one drive at a time (read-only)
sudo dd if=/dev/disk/by-id/ata-<model>_<serial> of=/dev/null bs=1M count=2000 status=progress
```

While this runs, one bay's activity LED will blink rapidly. Record which bay it is.

### Step 2: Match serial numbers

For each drive, confirm the serial number:

```bash
# SATA drives
sudo smartctl -i /dev/disk/by-id/ata-<model>_<serial> | grep "Serial Number"

# NVMe drives (if your DAS supports NVMe)
sudo smartctl -i /dev/disk/by-id/nvme-<model>_<serial> | grep "Serial Number"
```

You can also use `btrdasd config show` to display the target serials in your configuration.

### Step 3: Record your mapping

Use the template below. Adjust bay count and layout to match your enclosure.

## Bay Mapping Template

```
+------------------------------------------+
| <Your Enclosure Model> (front view)      |
+------------+------------+----------------+
|   Bay 1    |   Bay 2    |   Bay 3        |
| <serial-1> | <serial-2> | <serial-3>     |
| <capacity> | <capacity> | <capacity>     |
| <role>     | <role>     | <role>         |
+------------+------------+----------------+
|   Bay 4    |   Bay 5    |   Bay 6        |
| <serial-4> | <serial-5> | <serial-6>     |
| <capacity> | <capacity> | <capacity>     |
| <role>     | <role>     | <role>         |
+------------+------------+----------------+
```

Adjust the grid to match your enclosure's bay count and physical arrangement (2-bay, 4-bay, 6-bay, 8-bay, etc.).

## Drive Details Template

| Bay | Serial | Model | Size | Partitions | Role | BTRFS Label |
|-----|--------|-------|------|------------|------|-------------|
| 1 | `<serial>` | `<model>` | `<size>` | `<partition layout>` | `<role>` | `<label>` |
| 2 | `<serial>` | `<model>` | `<size>` | `<partition layout>` | `<role>` | `<label>` |
| ... | ... | ... | ... | ... | ... | ... |

### Roles

Common drive roles in a DAS backup configuration:

| Role | Description |
|------|-------------|
| **Primary Backup** | `role = "primary"` -- main btrbk target, receives all snapshot send/receive streams, including subvolumes adopted with no configured parent; holds the refreshed `@`/`@home` and their archives |
| **Bootable Recovery** | Has an ESP partition + its own, independent bootable OS. Configure it as `role = "mirror"`; nothing may ever write your system's ESP onto it |
| **Mirror** | `role = "mirror"` -- secondary target receiving the streams of sources whose `target_labels` is empty, with its own retention. The boot-subvolume refresh and archive pruning never touch it |
| **General Storage** | Non-critical data (RAID0 or single-drive) |
| **Cold Spare** | Unused drive kept ready as a replacement |

### Partition Layouts

Typical partition schemes for DAS backup drives:

- **Whole-disk BTRFS** -- best for pure backup targets (no ESP needed)
- **ESP + BTRFS** -- for bootable recovery drives (e.g., 1.5G FAT32 ESP + rest as BTRFS)
- **Whole-disk BTRFS RAID0** -- for expendable general storage arrays

## How Serials Map to config.toml

Each `[[target]]` entry in `/etc/das-backup/config.toml` identifies a drive by serial:

```toml
[[target]]
label = "primary-backup"
serials = ["<your-drive-serial>"]
mount_uuid = "<filesystem-uuid>"
mount = "/mnt/backup-primary"
role = "primary"

[target.retention]
daily = 7
weekly = 4
monthly = 12
yearly = 0
```

`serials` takes an array — a single-drive target lists one serial, a BTRFS RAID-1 target
lists both member serials (operator advisory only: a missing member logs a warning but
does not abort, since a degraded RAID-1 array still mounts from any present leg). Set
`mount_uuid` to mount by the filesystem's BTRFS UUID directly instead of resolving a device
from `serials` — every target on the author's system has one. With `mount_uuid` set, `backup-run.sh`,
`btrdasd backup run` and the GUI all treat the target as present when any listed serial is
attached or a filesystem with that UUID is on the host, mount it by UUID and verify the mount
by UUID. The setup wizard records it for a new target whose drive is attached;
`sudo btrdasd setup --check` reports every target without one and prints the UUID to add.

The backup scripts use `smartctl` to detect which `/dev/sdX` currently corresponds to each serial at runtime. This means your backup runs correctly regardless of device letter assignment.

`btrdasd setup` also renders the serials and `mount_uuid` into `/etc/udev/rules.d/99-das-backup-udisks-ignore.rules`, which hides the targets from udisks so no desktop session mounts them under `/run/media`. After changing a serial (a replaced drive), run `sudo btrdasd setup --upgrade` to regenerate it; `sudo btrdasd setup --check` confirms each attached target carries the flag.


## Re-cabling and moving the enclosure — POWER IT DOWN FIRST

**Always power the enclosure off before moving, reseating, or re-routing its USB
cable.** Pulling the cable on a live enclosure is not a hot-unplug the filesystem
can absorb.

```bash
# 1. Stop and mask the units that mount the enclosure so nothing (and no watchdog)
#    starts them mid-move.
sudo systemctl stop das-backup.service das-backup-full.service das-scrub.service
sudo systemctl mask das-backup.service das-backup-full.service das-scrub.service  # cachyos-sentinel WILL restart them otherwise
#    A job started from the GUI runs in btrdasd-helper, not in these units, and masking
#    does not stop it. Both checks must print nothing; WAIT means a job still holds the DAS:
sudo flock -n /run/das-backup.lock true || echo "WAIT: a backup is running"
sudo flock -n /run/das-maintenance.lock true || echo "WAIT: a backup, scrub, reconcile or doctor run holds the DAS"

# 2. Unmount everything the enclosure backs. Nothing should be under /run/media
#    (targets are hidden from udisks); a match there means the rule is not applying.
#    Confirm NOTHING is left mounted before touching the cable.
sudo umount /mnt/backup-22tb /mnt/backup-system-recovery-A /mnt/backup-system-recovery-B 2>/dev/null
mount | grep -E 'backup-22tb|backup-system-recovery|/run/media/bosco/das-' || echo "clear"

# 3. Power the enclosure OFF at its own switch. Then move the cable.

# 4. Power on, wait for enumeration, then re-register multi-device filesystems.
sudo btrfs device scan

# 5. Confirm the link came back at full rate BEFORE relying on it.
for d in /sys/bus/usb/devices/*/; do
  [ -f "$d/speed" ] && [ -f "$d/product" ] || continue
  printf '%-28s %s Mbit/s\n' "$(cat "$d/product")" "$(cat "$d/speed")"
done | sort -u        # enclosure should read 10000

# 6. Unmask and resume.
sudo systemctl unmask das-backup.service das-backup-full.service das-scrub.service
```

**What happens if you skip this.** On 2026-08-28 15:03 the cable was pulled while
udisks held all three backup filesystems mounted. Every one took a BTRFS emergency
shutdown, and the kernel kept stale device registrations that then *rejected the
returning disks* — `duplicate device ... scanned by (udev-worker)` — leaving the
array unmountable until the registrations were cleared with `btrfs device scan`.
No data was lost, but the array was offline until someone diagnosed it.

**Step 1 is not optional.** `cachyos-sentinel` auto-restarts any unit it observes
in `failed` state, so a plain `systemctl stop` is undone within seconds. Masking
makes the restart fail at the systemd layer instead. See
`.claude/rules/backup.md` § Sentinel Interaction.

## Maintenance

- **Update your mapping** whenever you add, remove, or rearrange drives
- **Verify after firmware updates** -- some DAS enclosures may re-order ports
- **Keep a printed copy** near the DAS for emergency reference

## Reference Example

See [examples/author-bay-mapping.md](examples/author-bay-mapping.md) for a fully documented 6-bay TerraMaster D6-320 configuration with specific drive models, serials, a BTRFS RAID-1 primary pair, and two bootable recovery drives.
