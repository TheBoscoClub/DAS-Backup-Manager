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
serial = "<your-drive-serial>"
mount = "/mnt/backup-primary"
role = "primary"

[target.retention]
weekly = 4
monthly = 12
```

The backup scripts use `smartctl` to detect which `/dev/sdX` currently corresponds to each serial at runtime. This means your backup runs correctly regardless of device letter assignment.

## Maintenance

- **Update your mapping** whenever you add, remove, or rearrange drives
- **Verify after firmware updates** -- some DAS enclosures may re-order ports
- **Keep a printed copy** near the DAS for emergency reference

## Reference Example

See [examples/author-bay-mapping.md](examples/author-bay-mapping.md) for a fully documented 6-bay TerraMaster D6-320 configuration with specific drive models, serials, RAID0 arrays, and bootable recovery drives.
