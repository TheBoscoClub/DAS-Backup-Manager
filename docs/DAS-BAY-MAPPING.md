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

Generate sustained I/O on one drive at a time and watch which bay's LED blinks:

```bash
# Replace /dev/sdX with each DAS drive letter in turn
sudo dd if=/dev/sdX of=/dev/null bs=1M count=2000 status=progress
```

While this runs, one bay's activity LED will blink rapidly. Record which bay it is.

### Step 2: Match serial numbers

For each drive, retrieve the serial number:

```bash
# SATA drives
sudo smartctl -i /dev/sdX | grep "Serial Number"

# NVMe drives (if your DAS supports NVMe)
sudo smartctl -i /dev/nvmeXn1 | grep "Serial Number"
```

You can also use `btrdasd config show` to display all detected target serials from your configuration.

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
| **Primary Backup** | Main btrbk target -- receives all snapshot send/receive streams |
| **Bootable Recovery** | Has an ESP partition + bootable OS -- can boot the system independently |
| **Mirror** | Redundant copy of another backup target |
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
does not abort, since a degraded RAID-1 array still mounts from any present leg). For a
multi-device target you can also set `mount_uuid` to mount by the filesystem's BTRFS UUID
directly instead of resolving a device from `serials`.

The backup scripts use `smartctl` to detect which `/dev/sdX` currently corresponds to each serial at runtime. This means your backup runs correctly regardless of device letter assignment.


## Re-cabling and moving the enclosure — POWER IT DOWN FIRST

**Always power the enclosure off before moving, reseating, or re-routing its USB
cable.** Pulling the cable on a live enclosure is not a hot-unplug the filesystem
can absorb.

```bash
# 1. Stop and mask the backup unit so nothing (and no watchdog) restarts it mid-move.
sudo systemctl stop das-backup.service
sudo systemctl mask das-backup.service      # cachyos-sentinel WILL restart it otherwise

# 2. Unmount everything the enclosure backs, including any udisks mounts a file
#    manager opened. Confirm NOTHING is left mounted before touching the cable.
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
sudo systemctl unmask das-backup.service
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

See [examples/author-bay-mapping.md](examples/author-bay-mapping.md) for a fully documented 6-bay TerraMaster D6-320 configuration with specific drive models, serials, RAID0 arrays, and bootable recovery drives.
