# Backup System Rules

## btrbk
- btrbk handles all BTRFS snapshot creation and send/receive
- Config at `/etc/btrbk/btrbk.conf`
- Never modify btrbk internals — use its CLI and config

## DAS Enclosure
- MediaSonic ProBox HF2-SU31C (4-bay USB-C)
- Bay mapping documented in `docs/DAS-BAY-MAPPING.md`
- DAS must be powered on and mounted before backup runs
- Drives are BTRFS-formatted, some RAID-0, some single

## Retention Policy
- **22TB target** (sdb/sdc RAID-0): 365 daily + 4 yearly snapshots
- **2TB targets** (sde, sdf): 4 weekly + 2 monthly snapshots
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
