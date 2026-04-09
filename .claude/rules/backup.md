# Backup System Rules

## btrbk
- btrbk handles all BTRFS snapshot creation and send/receive
- Config at `/etc/btrbk/btrbk.conf`
- Never modify btrbk internals — use its CLI and config

## DAS Enclosure
- TerraMaster D6-320 (6-bay USB-C, ASMedia 235c bridge)
- Bay mapping documented in `docs/DAS-BAY-MAPPING.md`
- DAS must be powered on and USB-C connected before backup runs
- Drives are BTRFS-formatted, single-device per bay
- Bays 1 & 3: 2TB emergency boot/recovery (independent OS, not btrbk targets)
- Bay 2: 22TB Exos primary backup target
- Bays 4-6: empty

## Retention Policy
- **22TB target** (primary-22tb, single Exos drive): 4 weekly + 12 monthly + 4 yearly snapshots
- Boot archives: 1 year retention, pruned by boot-archive-cleanup.sh

## Boot Subvolume Archival
- OLD behavior: delete @boot, recreate from live
- NEW behavior: snapshot to @.archive.YYYYMMDDTHHMMSS, then delete+recreate
- Archives are read-only snapshots on the backup target
- Cleanup runs after backup, prunes archives older than 365 days

## Email Reports
- SMTP config at `/etc/das-backup-email.conf` (mode 600)
- Reports include: btrbk status, throughput, archive/cleanup counts, indexing status
- Sent via msmtp/curl SMTP

## Content Indexer
- Database at `/var/lib/das-backup/backup-index.db`
- Span-based storage: unchanged files = 1 row across N snapshots
- FTS5 for full-text filename/path search
- Incremental indexing: only walk new snapshots
- Soft-fail: indexing errors don't abort the backup
