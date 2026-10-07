# DAS-Backup-Manager — Architecture

**Version**: 0.7.23.0

This document describes the system architecture, data flows, design decisions, and security posture of the DAS-Backup-Manager project.

## Scope

This project manages backups to **Direct-Attached Storage (DAS)** using the **BTRFS** filesystem. That's it. That's the scope.

The following are permanently out of scope and will never be added:

- **NAS** (Network-Attached Storage)
- **SAN** (Storage Area Network)
- **Cloud storage** (S3, Azure Blob, GCS, Backblaze, etc.)
- **Any filesystem other than BTRFS** (ext4, XFS, ZFS, NTFS, etc.)
- **Maintenance of non-backup (host-native) filesystems** — scrubbing, balancing, or
  otherwise servicing the host's own NVMe/SATA filesystems belongs to system-scope
  tooling (`btrfs-scrub@` timers), never to this project

The boundary is exclusive in both directions (user decision 2026-08-02): this project is
the **only** thing that scrubs the backup-media filesystems — their scrub must be tied to
the backup lifecycle (mounts, maintenance lock), which no calendar-driven system timer can
do — and this project never touches filesystems that aren't backup media.

Every architectural decision in this document — from the database schema to the installer templates — assumes DAS + BTRFS. This is not a general-purpose backup tool. Suggestions and contributions within this scope are very welcome.

## Every Subvolume Must Be Declared — `btrfs send` Does Not Descend

**A snapshot of a parent subvolume contains an empty directory wherever a child
subvolume is mounted.** The stream carries nothing, the run reports success, and
every observable signal agrees. This is the single easiest way to lose data with
this system, so it is stated here rather than left to be rediscovered.

Consequently `config.toml` declares each subvolume individually; being inside a
directory that is already backed up counts for nothing. Since 2026-10-01 the
backup run writes those entries itself (`btrdasd subvol sync`), so the
declaration cannot be forgotten. `btrdasd doctor --check-drift` stays as an
independent alarm on that mechanism: a subvolume it reports as missing means
sync failed, and the weekly timer is what makes it a control rather than a
good intention.

Found the hard way on 2026-09-01: `@srv/VirtualMachines`, created nested inside
the already-backed-up `@srv`, held a 137 GB VM image that appeared in no backup
for as long as it existed. A read-only snapshot of the parent showed **0 bytes**
at that path. `ClaudeCodeProjects/powershell-scripts` had been in the same state
for three weeks. See `.claude/rules/backup.md` for the reproduction and the
per-filesystem command that finds coverage gaps.

## Component Overview (v0.7.23.0)

```
┌─────────────────────────────────────────────────────────────┐
│                    User Space                               │
│  ┌──────────┐    ┌──────────────────────────────────────┐   │
│  │ btrdasd  │    │         btrdasd-gui (Qt6/KF6)        │   │
│  │  (CLI)   │    │  File Browser │ Config │ Monitor      │   │
│  └────┬─────┘    └──────────┬───────────────────────────┘   │
│       │    ┌────────────────┴──────────────┐                │
│       │    └────────────────┬──────────────┘                │
│  ┌────┴─────────────────────┴──────────────────────┐        │
│  │          libbuttered_dasd (Rust library)         │        │
│  │  adopt      │ backup     │ btrbk_conf │ caldate   │        │
│  │  config     │ db         │ doctor     │ expire    │        │
│  │  forget     │ fsutil     │ health     │ indexer   │        │
│  │  maintenance│ mount      │ progress   │ reconcile │        │
│  │  recovery_os│ report     │ restore    │ scanner   │        │
│  │  schedule   │ scrub      │ subvol                 │        │
│  └──────────────────────┬───────────────────────────┘        │
│                         │ D-Bus (org.dasbackup.Helper1)      │
│  ┌──────────────────────┴────────────────────────┐           │
│  │  btrdasd-helper (privileged daemon, polkit)   │           │
│  │  btrbk │ mount │ DB write │ config write      │           │
│  │  SMART │ systemd-timer │ btrfs commands       │           │
│  └───────────────────────────────────────────────┘           │
└──────────────────────────────────────────────────────────────┘
```

The system has six major components:

| Component | Language | Binary | Purpose |
|-----------|----------|--------|---------|
| Backup scripts | bash | N/A | btrbk orchestration, verification, boot archival |
| Rust library | Rust 2024 | `libbuttered_dasd.rlib` | 23 modules: single source of truth for all business logic |
| Content indexer / CLI | Rust 2024 | `btrdasd` | SQLite FTS5 database, full subcommand CLI |
| D-Bus privileged helper | Rust 2024 | `btrdasd-helper` | polkit-authorized daemon (20 methods, 7 polkit actions). No method accepts a path from the caller — the daemon reads `CANONICAL_CONFIG` only (since 0.7.20.0) and opens only the index database named in it (since 0.7.21.0) |
| KDE Plasma GUI | C++20 | `btrdasd-gui` | Full backup management: file browser, backup ops, health, config |
| Interactive installer | Rust 2024 | `btrdasd setup` | Config-driven 9-step setup wizard with template generation |

## Data Flow

### Backup Pipeline

```
1. systemd timer fires (das-backup.timer)
         │
         ▼
2. backup-run.sh (orchestrator)
         │
         ├──▶ singleton lock (/run/das-backup.lock: held → skip, exit 0; unusable → exit 1), then maintenance lock (/run/das-maintenance.lock, blocking)
         ├──▶ mount sources, verify_sources_before_write()   → every source volume is the expected filesystem
         │                                   (one already mounted, as fstab mounts /dasRaid0, is used as found)
         ├──▶ btrdasd subvol sync          → adopts new subvolumes, retires vanished ones, rewrites config.toml when the plan changes it, and btrbk.conf whenever it differs from what config.toml renders to
         │                                   (then: reload config, verify_sources_before_write() again for any source sync added;
         │                                    under --dryrun nothing is written and btrbk reads a temporary rendered btrbk.conf instead)
         ├──▶ create snapshot dirs, mount targets (by mount_uuid, else by serial), verify_targets_before_btrbk(), create target dirs
         ├──▶ btrbk run                    → one run_btrbk call: snapshots + send/receive to the backup targets (`btrbk dryrun` under --dryrun; --full changes only the boot-subvolume step below)
         ├──▶ btrdasd subvol expire        → deletes retired subvolumes' backups past their window, while the targets are still mounted (a dry run only, when this run's sync failed)
         ├──▶ btrdasd recovery-os status   → reads each mounted mirror target's own OS under @ (never writes to it), including what it starts at boot and whether that runs btrbk; STALE, or a WARNING that its boot will or may run btrbk, is a WARN, not a FAIL; records the reading in recovery-os.json for `btrdasd health` (not under --dryrun)
         ├──▶ update_boot_subvolumes()     → creates missing @/@home on non-mirror targets; archives + recreates them only on --full runs
         ├──▶ btrdasd walk                 → indexes new snapshots on the primary target into SQLite
         ├──▶ growth log, boot-archive-cleanup.sh → prunes expired @.archive.*/@home.archive.* snapshots
         ├──▶ capture_report_data()        → capacity, growth and latest-snapshot data read while the targets are still mounted;
         │                                   decide_run_counts() then settles the snapshot counts, before the run status
         ├──▶ unmount_all()                → each target unmount retried 5 times, 2 s apart; a target left mounted is a FAIL in the report,
         │                                   and so is one the mountpoint probe cannot tell about (unmounted anyway): "DAS can be safely
         │                                   disconnected" only on the gate's OK, else NOT safe and why (bd DAS-Backup-Manager-jug6);
         │                                   of the sources, only the mount points this run mounted, each once (one that will not
         │                                   unmount is a WARN with umount's message) — never one it found mounted
         │                                   (bd DAS-Backup-Manager-8cf)
         ├──▶ mailx                        → sends email report (local relay, 127.0.0.1:25), bounded: TERM after 60 s, KILL 10 s later;
         │                                   report written to last_report first (a write that fails is logged, and the journal has it)
         └──▶ btrdasd backup record-run    → adds the run to backup_runs, an uncountable snapshot count as NULL (--counts-unknown)
```

The report goes out before the run is recorded, because the record carries the report's own
outcome (a delivery failure fails the run). If recording then fails, the run is missing from the
history, so the record step marks the run FAIL (`run_history`) and writes and sends the report
again: `FAILURES DETECTED`, with a `RUN HISTORY` section saying the run is not recorded and why.

A run that aborts before its report (exit 3: no primary target, a target or source failing
verification, a command failing under `set -e`) never reaches those steps. Its EXIT trap,
`cleanup()`, records it as failed (`--counts-unknown`, the reason in the errors), sends one short
report with the subject `ABORTED` through the same relay path, and only then unmounts — the
unmount can hang on a drive that went away. A dry run sends and records nothing. A stop by a
signal (HUP, INT, USR1, PIPE, ALRM, TERM) keeps its own code (128 + its number), is recorded as
a stop once the run holds the maintenance lock, and sends no report.

A `--dryrun` stops after the expiry preview and the recovery OS check: it previews the archive pruner and
unmounts, and sends, records and archives nothing. It may create a missing, empty
target directory (for example for a pending adoption), as the real run would.

`btrdasd backup run` and the GUI (through `btrdasd-helper`'s `BackupRun`, which takes the
ticked sources, targets, a mode (`full` or `incremental`) and a steps dictionary of exactly
five booleans: `snapshot`, `send`, `boot_archive`, `index`, `email`) run the same
job through one library entry point, `backup::run_backup_job`: locks (decline if a
backup holds the singleton, wait for the maintenance lock), mount sources, subvolume
sync, mount targets (`mount::verify_write_targets()` before btrbk), `run_backup`, capture
the report data while mounted, unmount (same 5 × 2 s retry; a mount point left mounted
fails the run with `still mounted: …`), then report and record. A failed sync never
stops the run, but marks it failed in the result, the report and `backup_runs`. These
steps run only in `backup-run.sh`, which the timers start: `btrdasd subvol expire`,
the boot-archive pruner, the growth log, and the throughput log with its USB link
check (see `THROUGHPUT-BASELINE.md`). In the helper, a job's
progress goes through one ordered queue (`progress::OrderedProgress`) and ends with
exactly one `JobFinished`; `JobCancel` stops the reporting, not the work — a running
btrbk send is not interrupted, and the locks are held until it ends.

ESP synchronization to recovery drives was removed 2026-04-10 after the ESP-overwrite
incident — see `.claude/rules/esp-safety.md`. `backup-run.sh` has no ESP/rsync step.

Both orchestrators verify their targets before invoking btrbk, and they must:
`backup-run.sh` via `verify_targets_before_btrbk()`, the Rust path (D-Bus/GUI and
manual `btrdasd`) via `mount::verify_write_targets()`. The Rust guard was missing
until 0.7.21.0, and `run_backup` deliberately skips re-checking mount status when
targets are named explicitly — which the GUI always does — so `bd DAS-Backup-Manager-9on`
was reachable from the GUI for as long as the GUI has existed
(`bd DAS-Backup-Manager-aea`).

**Sources are verified too, since 0.7.22.1.** `verify_sources_before_write()` runs
between `mount_sources` and `create_snapshot_dirs` — the latter being the first
thing in the run that writes to a source path. Per source it requires a real
mountpoint, a filesystem UUID matching what the source's `device` resolves to, and
`FSROOT=/`, the top-level view that `mount -o subvolid=5` produces and that
btrbk.conf's subvolume paths are relative to.

The mountpoint check is not redundant with the UUID check, and the reason is worth
keeping: the `nvme` source's filesystem **is** the root filesystem — the source is
merely its `subvolid=5` view — so on a bare `/.btrfs-nvme` the fallthrough resolves
to the same UUID and a UUID-only guard passes. Verified on the live host: with
`mountpoint -q /.btrfs-nvme` false, `findmnt --target` still reported the expected
UUID. A regression case exists specifically to go red if anyone "simplifies" the
guard down to the UUID comparison.

Two sources declare `device` as a path rather than `UUID=`, and for those this is a
**consistency check, not an identity check** — the expected UUID is resolved from the
device node at verification time, which cannot detect that the path now refers to a
different disk. `/dev/nvme1n1p2` is also one leg of a two-device RAID-1, so it names
a *member*, not a filesystem. The remedy is a config change (declare them by `UUID=`),
not a code change; until then the mountpoint check is their only guarantee — and it is
the one that catches the defect this guard was written for
(`bd DAS-Backup-Manager-zlv`).

### Scrub Pipeline

```
1. systemd timer fires (das-scrub.timer, monthly OnCalendar from [scrub].on_calendar)
         │
         ▼
2. btrdasd scrub run (indexer/src/scrub.rs)
         │
         ├──▶ /run/das-scrub.lock         non-blocking singleton — a second pass skips, never queues
         ├──▶ /run/das-maintenance.lock   blocking, shared with backup-run.sh — deferral, not cancellation
         ├──▶ mount + resume-or-start     sequential, per configured [scrub].targets
         └──▶ /var/lib/das-backup/scrub-state.json   consumed by health checks
```

**Resume-or-start** (bd `DAS-Backup-Manager-292`): each target does not blindly `btrfs scrub start`.
`decide_scrub_start_mode` reads the prior `/var/lib/btrfs/scrub.status.<fsuuid>` record and, when it
is `Aborted` (`canceled:0 finished:0` — what a reboot or an unmount that kills a scrub mid-write
leaves behind) *and* the kernel confirms no scrub is running, issues `btrfs scrub resume -B` to
continue from the saved position rather than restarting from zero. A `finished` or deliberately
`canceled` record, a running scrub, or unreadable liveness all fall through to a plain start — the
decision never acts on a guess. If `resume` finds no resumable state after all ("nothing to resume"),
the runner falls back to `btrfs scrub start -B -f`, which also clears a genuinely stale record. This
matters specifically because the DAS targets are unmounted between backup runs: the position record
lives on the host root keyed by FS UUID, so it survives the unmount, and the next monthly pass
finishes what an interruption left undone instead of silently abandoning it. The decision is a pure
`decide_from(outcome, live_state)` function so every combination is unit-testable; end-to-end resume
across an unmount/remount is proven by `indexer/tests/scrub_loopback.rs`.

`das-scrub.service` is deliberately dumb (`Type=oneshot`, unbounded `TimeoutStartSec=infinity`,
no `Conflicts=`, no `ExecStopPost` cancel, no `RuntimeMaxSec=`). All ordering against a running
backup is enforced by the engine's own blocking maintenance lock — identical to a manual
`btrdasd scrub run` invocation, so there is no unit-layer special case. The timer default
(`*-*-01 03:05:00`, since 2026-08-02) deliberately fires five minutes after the 03:00 daily
backup on the 1st: the backup already holds the maintenance lock, so the scrub blocks and then
starts the moment the backup finishes — and if a long scrub is still running when the *next*
day's 03:00 backup fires, the same lock holds that backup until the scrub completes (deferral,
never a skipped backup). The three DAS filesystems are scrubbed **sequentially** in
`[scrub].targets` list order because they share one USB path; the host's native NVMe/SATA
filesystems are scrubbed by the system-scope `btrfs-scrub@` timers outside this project and may
run in parallel with each other and with this engine. `[scrub].enabled` gates
only whether `das-scrub.timer` is enabled by `btrdasd setup`; the engine still honors a manual
run regardless (warn-only). See `.claude/rules/backup.md` and `docs/SCRUB-SCHEDULING-PROPOSAL.md`
for the full design rationale.

**Operational note — a caught-up boot scrub can legitimately delay a backup by hours.**
`das-scrub.timer` sets `Persistent=true`, so a monthly fire missed while the host was off (or the
DAS enclosure was disconnected) runs as soon as the timer unit is next active, not just at the next
`OnCalendar=` match. Combined with the service's unbounded `TimeoutStartSec`, a catch-up scrub that
starts shortly before `das-backup.timer` fires can still be mid-pass (a 22 TB RAID-1 pass measured
~8h53m in production) when the backup would normally start. The blocking maintenance lock makes the
backup wait rather than collide with the scrub — `backup-run.sh` defers, it does not fail — so the
visible symptom is a backup that starts hours late with `journalctl` "waiting on maintenance lock"
lines in between, not a missed or broken backup. This is the designed trade-off (deferral over data
races), not a bug; see the "Locking" section of the `scrub` module doc comment
(`indexer/src/scrub.rs`) for the lock-acquisition order.

**Health integration** (`indexer/src/health.rs`, bd `DAS-Backup-Manager-5kb`): `TargetHealth::scrub`
reports per-filesystem age (days since the last `finished`, zero-error scrub), WARN/FAIL against
`[scrub].warn_age_days` / `fail_age_days`, and an immediate FAIL for any aborted/canceled/errored
latest attempt or nonzero error counter — regardless of how recent or clean an earlier success was.
It reads **only** `scrub-state.json` (mode `0644`), never the raw per-device
`/var/lib/btrfs/scrub.status.<fsuuid>` record (mode `0600`, root-only, and the exact record type
that made an aborted `system-recovery-A` scrub look healthy for 64 days). This keeps `btrdasd health`
(any user) and the GUI's `HealthQuery()` (root, via `btrdasd-helper`) in agreement — see the
`ScrubHealth` doc comment in `health.rs` for the full rationale.

### Indexing Pipeline

```
btrdasd walk /mnt/backup-target
         │
         ├──▶ discover_snapshots()       Scan target for source/name.timestamp dirs
         │         │                     Parse dirname with regex: ^(.+)\.(\d{8}T\d{4,6})$
         │         ▼
         │    Filter out already-indexed snapshots (by source, name and timestamp —
         │    a copy on another target is recorded in snapshot_targets, not re-indexed)
         │
         ├──▶ For each new snapshot:
         │         │
         │         ├──▶ scan_directory()   walkdir recursive traversal (soft-fail on errors)
         │         │         │
         │         │         ▼
         │         │    Vec<FileEntry> { path, name, size, mtime, file_type }
         │         │
         │         └──▶ index_snapshot()   Span-based deduplication:
         │                   │              - Unchanged file → extend span (last_snap = new)
         │                   │              - Changed file   → upsert file, new span
         │                   │              - New file       → insert file + span
         │                   ▼
         │              SQLite DB updated
         │
         └──▶ Print summary (discovered/indexed/skipped + per-snapshot stats)
```

### GUI Read Path

```
btrdasd-gui
         │
         ├──▶ DBusClient (org.dasbackup.Helper1)
         │         │
         │         ├──▶ IndexListSnapshots()  → SnapshotModel (tree: date groups → snapshots)
         │         ├──▶ IndexListFiles()      → FileModel (paginated, 10k per page)
         │         ├──▶ IndexSearch()          → SearchModel (FTS5 results with span info)
         │         ├──▶ IndexStats()           → Stats display
         │         ├──▶ IndexBackupHistory()   → BackupHistoryView
         │         ├──▶ IndexSnapshotPath()    → Snapshot path resolution
         │         ├──▶ HealthQuery()          → HealthDashboard (SMART, growth, services)
         │         └──▶ ConfigGet()            → BackupPanel, ConfigDialog
         │
         ├──▶ SnapshotTimeline            Custom QPainter widget (visual timeline)
         ├──▶ SnapshotWatcher             QFileSystemWatcher → auto-detect new snapshots
         ├──▶ IndexRunner                 D-Bus IndexWalk → trigger index walk
         └──▶ RestoreAction               KIO::copy file restore with destination chooser
```

## Database Architecture

### Schema

The SQLite database at `/var/lib/das-backup/backup-index.db` (schema version 4, kept in
`PRAGMA user_version`) uses four index tables, two history tables and an FTS5 virtual table:

```sql
snapshots (id PK, name, ts, source, path UNIQUE, indexed_at)
files     (id PK, series, path, name, size, mtime, type)  -- UNIQUE(series, path); series = snapshot name + source
spans     (file_id FK, first_snap FK, last_snap FK, PK(file_id, first_snap))
snapshot_targets (snapshot_id FK ON DELETE CASCADE, target_root, path,
                  PK(snapshot_id, target_root))   -- schema v3, which targets hold a snapshot
files_fts (FTS5 virtual: name, path — synced via triggers)
backup_runs  (id PK, timestamp, success, mode, snaps_created, snaps_sent, bytes_sent,
              duration_secs, errors)              -- run history, read by the GUI; schema v4: a
                                                  -- count the run could not take is NULL
target_usage (id PK, timestamp, target_label, total_bytes, used_bytes, snapshot_count)
```

### Span-Based Deduplication

Instead of recording every file in every snapshot (which would produce millions of rows), spans compress consecutive identical file appearances:

```
File: /home/user/document.pdf (unchanged across snapshots 5–12)

Naive model:  8 rows in a join table (1 per snapshot)
Span model:   1 row → spans(file_id=42, first_snap=5, last_snap=12)
```

When snapshot 13 arrives:
- If the file is unchanged → `UPDATE spans SET last_snap=13` (extend)
- If the file changed → new span `(file_id=42, first_snap=13, last_snap=13)` + update file metadata
- If the file is gone → no action (span accurately records last appearance)

### FTS5 Synchronization

Three triggers keep the FTS5 index consistent:

| Trigger | Event | Action |
|---------|-------|--------|
| `files_ai` | `AFTER INSERT ON files` | Insert into FTS5 |
| `files_ad` | `AFTER DELETE ON files` | Delete from FTS5 |
| `files_au` | `AFTER UPDATE ON files` | Delete old + insert new in FTS5 |

### Performance Indexes

| Index | Columns | Query Pattern |
|-------|---------|---------------|
| `idx_snapshots_source_name` | (source, name) | Group snapshots by source during walk |
| `idx_snapshots_ts` | (ts) | Chronological ordering |
| `idx_spans_file_id` | (file_id) | Lookup spans for a file |
| `idx_files_name` | (name) | Direct file name lookups |
| `idx_files_series_path` | (series, path) UNIQUE | File deduplication within one subvolume series |
| `idx_spans_last` | (last_snap) | Span extension queries |
| `idx_snapshot_targets_snap` | (snapshot_id) | Which targets hold a snapshot |
| `idx_backup_runs_ts` | (timestamp) | Run history ordering |
| `idx_target_usage_label_ts` | (target_label, timestamp) | Usage history per target |

### Concurrent Access

- **WAL journal mode** enables simultaneous reads and writes
- The indexer (`btrdasd walk`) holds a write connection
- The GUI accesses the database through `btrdasd-helper` D-Bus methods (no direct database connection)
- `PRAGMA optimize` runs on connection close (via Rust `Drop` impl)

## Installer Architecture

### Config-Driven Design

The installer uses a TOML configuration file (`/etc/das-backup/config.toml`) as the single source of truth:

```
wizard → Config struct → config.toml (save)
                              │
                              ▼
                    GeneratedFiles::generate()
                              │
                              ▼
                    installer::install() → write files + manifest
```

Every file setup writes is replaced whole — `fsutil::write_atomic_mode`, which every writer of
`config.toml` and `btrbk.conf` uses too: a temp file of the write's own (`.NAME.PID.N.tmp`, made only
under a free name), given its mode (scripts 0755, any other file the mode it had) and, written by
root, the old file's owner and group, flushed, renamed over the old one, the directory flushed — so
a backup already reading a script keeps the old file, and two writers never meet. Every mode that writes or removes these files (install,
`--modify`, `--force`, `--upgrade`, `--uninstall`, `--uninstall-all`) does it holding
`/run/das-backup.lock` and then `/run/das-maintenance.lock`, taken without waiting before the first
write, or refuses with exit 75 on stderr, changing nothing; `--modify` also refuses when
`config.toml` changed while its wizard was open. `install`, `uninstall` and `uninstall_all` take the
proof of that hold (`installer::SetupLocks`) as an argument, and `setup::dispatch` reaches the host
only through `SetupHost`, so tests drive every mode on a scratch tree.

### Config Sections

| Section | Fields | Purpose |
|---------|--------|---------|
| `general` | version, install_prefix, db_path, log_file, growth_log, last_report, btrbk_conf | Global settings |
| `init` | system (systemd/sysvinit/openrc) | Init system selection |
| `schedule` | incremental, full, randomized_delay_min | Backup timing |
| `das` | model_pattern, io_scheduler, mount_opts | DAS enclosure match, I/O scheduler, target mount options |
| `boot` | enabled, subvolumes[], archive_retention_days | Boot subvolume archival + pruning (`boot-archive-cleanup.sh`) |
| `scrub` | enabled, on_calendar, targets[], warn_age_days, fail_age_days | Scheduled BTRFS scrub (`btrdasd scrub`, `das-scrub.timer`) |
| `doctor` | exclude[] | Older exclusion list, still read and merged with `[subvolumes].exclude` |
| `subvolumes` | exclude[] | Glob patterns the backup run must not adopt (`btrdasd subvol sync`); a pattern also covers everything nested under it |
| `restore` | allowed_roots[] | Where a restore may write (an unoverridable denylist is checked first) |
| `recovery_os` | max_age_days | Age past which a mirror target's own OS is reported STALE (`btrdasd recovery-os`) |
| `source[]` | label, volume, device, snapshot_dir, target_subdirs[], target_labels[], subvolumes[] (name, manual_only, snapshot_name, adopted, retired) | BTRFS sources; `adopted` / `retired` dates are written by the backup run; empty `target_labels` = every target |
| `target[]` | label, serials[], mount_uuid, mount, role (primary/mirror), display_name, retention (daily, weekly, monthly, yearly) | Backup targets; mounted by `mount_uuid` when set, else by serial |
| `email` | enabled, smtp_host, smtp_port, from, to | Email reports |
| `gui` | enabled (bool) | GUI installation toggle |

**Removed section**: `[esp]` (enabled, source_path, method, packages[]) — ESP/boot mirroring
was removed from the codebase in two steps: the orphan hook generator on 2026-04-10, then the
remaining `Esp` struct and `sync_esp()` on 2026-04-12 (see `.claude/rules/esp-safety.md`). Old
`config.toml` files with a leftover `[esp]` section are silently ignored by serde on load.

### Template Engine

Templates are rendered programmatically (no external template files):

| Function | Output | Description |
|----------|--------|-------------|
| `btrbk_conf::render_btrbk_conf()` | `btrbk.conf` | Per-source volume blocks with target retention; retired entries are left out. In the library because the backup run, the `subvol` commands and the GUI helper's saves regenerate it too |
| `render_systemd_service()` / `render_systemd_timer()` | `das-backup{,-full}.{service,timer}` | ExecStart with full flag support, `SuccessExitStatus=3` (a run that began and failed); OnCalendar with RandomizedDelaySec |
| `render_systemd_scrub_service()` / `render_systemd_scrub_timer()` | `das-scrub.{service,timer}` | Monthly scrub from `[scrub].on_calendar` |
| `render_systemd_doctor_service()` / `render_systemd_doctor_timer()` | `das-backup-doctor.{service,timer}` | Weekly drift check, `SuccessExitStatus=1` |
| `render_udev_udisks_ignore()` | `/etc/udev/rules.d/99-das-backup-udisks-ignore.rules` | Hides every target from udisks2 by serial and `mount_uuid` |
| `render_cron_entry()` | cron lines | For sysvinit/OpenRC systems |
| embedded scripts (`include_str!`) | `backup-run.sh`, `backup-verify.sh`, `boot-archive-cleanup.sh` | Real production scripts installed flat at `${prefix}/lib/das-backup/` (same layout as cmake's install) |

No email config file is generated because this project stores no mail
credential: reports are submitted unauthenticated to the local relay named by
`[email].smtp_host`/`smtp_port`. ESP sync hook generation was removed
2026-04-10 (see `.claude/rules/esp-safety.md`).

The generated units land in `/etc/systemd/system/`, and they are the only copies: CMake installs
just `btrdasd-helper.service`. Until bd `DAS-Backup-Manager-7rf` it also installed its own
`das-backup{,-full}.{service,timer}` under `<prefix>/lib/systemd/system` — a second writer of the
same units, which systemd ran whenever setup's were gone, with a USB-glob condition and a
six-hour timeout setup's never had. `setup --upgrade` removes the copies older versions left under
`/usr` and `/usr/local`, only those whose bytes are a version this project installed
(`src/setup/retired_units.rs`). Every service is ordered
`After=local-fs.target`; the backup and scrub services are also ordered after
`time-sync.target`, because retirement and expiry dates come from the clock.
On this host (live config, 2026-10-02):

| Unit | Schedule | Runs |
|------|----------|------|
| `das-backup.timer` | daily 03:00, up to 30 min random delay | `backup-run.sh` |
| `das-backup-full.timer` | Sunday 04:00, up to 30 min random delay | `backup-run.sh --full` |
| `das-scrub.timer` | 1st of the month 03:05, no random delay | `btrdasd scrub run` |
| `das-backup-doctor.timer` | Sunday 02:00 | `btrdasd doctor --check-drift --email` |

All four timers are `Persistent=true`. `btrdasd-helper.service` (`Type=dbus`,
bus name `org.dasbackup.Helper1`) is installed by CMake, not generated; it is
enabled on this host and is also D-Bus-activatable.

### System Detection

The `detect` module auto-discovers the host environment:

| Detection | Method | Output |
|-----------|--------|--------|
| Block devices | `lsblk --json` parsing | USB/SATA/NVMe devices with size, serial, partitions |
| BTRFS subvolumes | `btrfs subvolume list` parsing | Subvolume paths and IDs |
| Init system | `which systemctl`, `which rc-service`, `/etc/init.d` existence | systemd/openrc/sysvinit (systemd if none match) |
| Package manager | Binary existence checks | pacman/apt/dnf/zypper/apk |
| Dependencies | `which` checks for btrbk, btrfs, smartctl, etc. | Missing dependency report |

### Manifest Tracking

Every install writes `/etc/das-backup/.manifest` — a plain-text list of generated file paths. This enables:
- `--uninstall`: Remove exactly the files that were installed
- `--upgrade`: Regenerate the same files from updated config
- `--check`: Verify all manifest files exist (presence only, not content), alongside config validation, relay reachability, targets without `mount_uuid`, the udisks hiding read back from udev, and dependencies


## Privilege Boundary

`btrdasd-helper` is a root daemon on the system bus, reachable by unprivileged local clients. Polkit answers *may this caller perform this action* — it says nothing about *which object*. Four rules follow. The first three were introduced in 0.7.20.0 after an independent review found each one violated; 0.7.21.0 found rule 1 violated a second time in a different parameter, and added rule 4:

1. **No path arrives from the caller.** The daemon reads and writes exactly one configuration file, `CANONICAL_CONFIG`, and opens exactly one index database, `canonical_db_path()` (resolved from `general.db_path` in that config). Seventeen methods previously took a `config_path` and passed it to `Config::load`/`save` as root, so `ConfigGet` — whose action `org.dasbackup.config.read` the installed policy grants to any active session with **no prompt**, so the GUI can list sources at startup — doubled as an unauthenticated root-privileged read of any TOML-parseable file (`bd DAS-Backup-Manager-wd7`).

   The same defect survived one indirection further out until 0.7.21.0: seven `Index*` methods took a `db_path` and handed it to `Database::open` as root. That is **not a read** — `Connection::open` creates the file when absent, `journal_mode=wal` creates `-wal`/`-shm` sidecars, and `execute_batch(SCHEMA_SQL)` + `migrate()` write into it, so an existing SQLite database anywhere on the host would be opened and *migrated*. Six of the seven sit behind `org.dasbackup.index.read`, which is `allow_active=yes` — **no authentication prompt at all** (`bd DAS-Backup-Manager-gko`). Fixing one parameter did not fix the class; a path parameter on a root daemon is the defect, whatever it is called.
2. **A destination is policy, not a parameter.** Restore paths are checked against `[restore] allowed_roots` and an unoverridable denylist before anything is created, and writes use `O_NOFOLLOW` (`bd DAS-Backup-Manager-s05`). The denylist covers the paths where a restored file becomes executable code or changes system identity, and since 0.7.22.0 also `/srv/http` and `/srv/ftp` — a file restored into a document root is *served*, which is the same property reached by a network path rather than an exec path.
3. **Authorization of an action is not authorization over an object.** `JobCancel` checks polkit *and* that the caller owns the job; job ids are broadcast on every progress signal, so the action check alone let any authorized client abort anyone's work (`bd DAS-Backup-Manager-h2s`).
4. **A source is policy too, not just a destination.** Rule 2 constrained where a restore may *write* and left unconstrained where it may *read*, so an authorized caller could have root copy `/etc`, `/root`, or another user's home into a permitted destination and then read it unprivileged. `restore::check_source_allowed()` requires the snapshot to resolve inside a configured backup target before anything is read, and fails closed when no targets are configured (`bd DAS-Backup-Manager-7ra`).

The daemon also participates in the same singleton + maintenance lock interlock as `backup-run.sh` and the scrub engine. It did not until 0.7.20.0: `bd DAS-Backup-Manager-pe6` fixed the CLI in 0.7.15.0 and never reached the daemon the GUI actually calls (`bd DAS-Backup-Manager-dca`). The singleton locks are `/run/das-backup.lock` (backup, any path), `/run/das-scrub.lock`, `/run/das-doctor.lock` and `/run/das-reconcile.lock`, all non-blocking. Every side takes its singleton first, then the shared `/run/das-maintenance.lock`: backup and scrub wait for it, doctor and reconcile defer if it is held. The `IndexWalk`, `RestoreFiles` and `RestoreSnapshot` jobs take only the maintenance lock, as the CLI's `walk` and `restore` do (`bd DAS-Backup-Manager-frb`): they wait for it, logging one line that names the holder, and a job cancelled while it waits ends `cancelled` at once, having mounted nothing. Mounting a target needs the proof that the lock is held — `mount::ensure_targets_mounted` takes a `maintenance::MaintenanceHeld` — so no path can skip it.

## Build System

### CMake + ExternalProject

The root `CMakeLists.txt` orchestrates both C++ and Rust builds:

```cmake
option(BUILD_GUI     "Build KDE Plasma GUI (requires Qt6/KF6)" ON)
option(BUILD_INDEXER "Build btrdasd Rust binary via cargo"      ON)
option(BUILD_HELPER  "Build btrdasd-helper D-Bus daemon"        ON)
```

- `BUILD_INDEXER=ON`: Uses `ExternalProject_Add` to invoke `cargo build --release` for all Rust targets
- `BUILD_HELPER=ON`: Builds `btrdasd-helper` D-Bus daemon and installs polkit/D-Bus configuration
- `BUILD_GUI=ON`: Uses `add_subdirectory(gui)` with standard KDE/Qt6 CMake modules
- `BUILD_GUI=OFF`: Skips Qt6/KF6 dependency entirely (headless CLI-only build)

## Security & Design Decisions

### Memory Safety

**Rust for the indexer**: The content indexer processes untrusted filesystem data (file paths, names, sizes from backup snapshots). Rust eliminates buffer overflows, use-after-free, and data races at compile time. `unsafe` is confined to single-call `libc` FFI wrappers — `geteuid`, `flock`, `statvfs`, `syncfs`, `gethostname`, `time`/`localtime_r` — plus `std::env::set_var`/`remove_var` in tests; `btrdasd-helper` has none.

**C++20 for the GUI**: The GUI uses Qt6/KF6 which requires C++. It holds no database connection and no privileged code — everything goes through `DBusClient`. Its components use:
- Qt parent-child ownership for widget and object lifetime (objects are created with `new` and a parent, and released with `deleteLater()`; the one manual `delete` frees layout items Qt has handed back from a layout)
- Compiled with `-Wall -Wextra -Wpedantic -Werror` — zero warnings policy

**Bash scripts**: All scripts use `set -euo pipefail` for fail-fast behavior.

### SQL Injection Prevention

Every database operation uses parameterized prepared statements exclusively:

```rust
// Rust (rusqlite) — parameterized query
conn.query_row("SELECT id FROM snapshots WHERE path = ?1", params![path], |row| row.get(0))
```

The GUI issues no SQL at all; every query runs in the Rust library, behind `btrdasd-helper`.
No string concatenation is used to build SQL queries anywhere in the codebase.

### File Permission Hardening

| File | Mode | Owner | Reason |
|------|------|-------|--------|
| `/etc/das-backup/config.toml` | `0o644` | root | No secrets — mail submission is unauthenticated; the relay's upstream key lives in `/etc/postfix/sasl_passwd` (root-only) |
| Generated scripts | `0o755` | root | Executable by system |
| `/var/lib/das-backup/backup-index.db` | `0o644` | root | Writable by the indexer (root); readable by any local user — the GUI itself reads it only through `btrdasd-helper` |

### Database Integrity

- **WAL journal mode**: Prevents corruption from concurrent access, provides atomic commits
- **Foreign keys enabled**: `PRAGMA foreign_keys = ON` enforces referential integrity between spans/files/snapshots
- **PRAGMA optimize on close**: the Rust `Database` `Drop` impl runs `PRAGMA optimize` to maintain query planner statistics (the GUI has no database connection)
- **Unique constraints**: File paths (per subvolume series) and snapshot paths have unique indexes preventing duplicates

### Input Validation

- **Snapshot names**: Validated with regex `^(.+)\.(\d{8}T\d{4,6})$` — rejects malformed directories
- **TOML configuration**: Deserialized via `serde` with strongly-typed structs — invalid config fails at parse time
- **FTS5 queries**: `Database::search` wraps a bare search term in `"` delimiters so punctuation is literal; a query containing `*`, `:` or `"` is passed through as FTS5 syntax. Either way it is a bound parameter, so a malformed query is an FTS5 error, never SQL
- **File paths**: All path operations use `std::path::PathBuf` (Rust) or `QString` (C++) — no raw C string manipulation
- **Restore sources and destinations**: Both ends are checked against policy before anything is read or created, on the fully resolved path so a symlinked ancestor cannot smuggle either past the check — destinations against `[restore] allowed_roots` plus an unoverridable denylist, sources against the configured target mounts
- **Backup write targets**: `mount::verify_write_targets()` asserts every target btrbk will be told to write to is a real mount point carrying the expected filesystem UUID, before btrbk is invoked. Writing to a bare mount point falls through to the underlying filesystem — normally the NVMe root — and fills it (`bd DAS-Backup-Manager-9on`)

### Efficiency

- **Span-based deduplication**: A file unchanged across 100 snapshots = 1 file row + 1 span row (not 100 rows)
- **Incremental indexing**: `btrdasd walk` skips already-indexed snapshots (checked by source, name and timestamp)
- **Performance indexes**: 9 targeted indexes for common query patterns (see Database Architecture)
- **Bundled SQLite**: Compiled from source with FTS5 enabled — no dependency on system SQLite version

### Stability

- **Soft-fail indexing**: In `backup-run.sh`, indexing errors are logged but never abort the backup. The backup pipeline always completes regardless of indexer state.
- **Error propagation**: Rust `?` operator propagates errors cleanly up the call chain with descriptive error types
- **Graceful degradation**: The GUI has no database connection of its own. If `btrdasd-helper` cannot be reached, `DBusClient` reports it once and the GUI still launches with an empty state.

### Privacy

- **Metadata only**: The database stores file paths, names, sizes, and timestamps — never file contents. No user data is read or stored beyond filesystem metadata.
- **No email credential at all** (since 2026-08-06): reports are submitted unauthenticated over loopback to a local mail relay, which holds the upstream key under `/etc` and authenticates by envelope sender. Neither sender reads, stores, or transmits a password. This also closed a real exposure — both paths previously placed the SMTP password on a child process's command line, readable in `/proc/<pid>/cmdline` by any local process for the duration of the send.
- **No telemetry**: No analytics or usage tracking of any kind. The only outbound network activity is the backup report, and only as far as `127.0.0.1:25`; forwarding it off-host is the relay's job, and its provider sees report contents (drive serials, capacities, SMART status, snapshot paths).

### Database Encryption Assessment

The backup index database stores file paths (e.g., `/home/user/Documents/tax-return-2025.pdf`), names, sizes, and timestamps. While no file contents are stored, file paths can reveal personal information about what files exist on a system.

**Current protections**:
- Filesystem permissions: root-owned, mode 0644
- Database lives at `/var/lib/das-backup/backup-index.db` — local filesystem only
- Not network-exposed in any configuration

**For high-sensitivity deployments**, SQLCipher provides full-database AES-256-CBC encryption:

```toml
# In indexer/Cargo.toml, replace:
rusqlite = { version = "0.40", features = ["bundled"] }
# With:
rusqlite = { version = "0.40", features = ["bundled-sqlcipher"] }
```

This requires a passphrase on every database open (the indexer and `btrdasd-helper`), adds approximately 30% query overhead, and increases the binary size.

**Recommendation**: Not enabled by default. Most backup systems (btrbk, rsnapshot, restic, borgbackup) do not encrypt their metadata indexes. The database is local-only; at mode `0644` it is readable by every local user, not only root. For users with high-sensitivity requirements (e.g., shared systems, compliance mandates), the SQLCipher path is documented and straightforward to enable.

## Module Reference

### Rust Indexer (`indexer/src/`)

| Module | File | Lines | Purpose |
|--------|------|-------|---------|
| `adopt` | `src/adopt.rs` | ~2600 | Subvolume sync: lists each verified source volume, adopts new subvolumes, retires and revives entries, replaces `config.toml` and `btrbk.conf` together (`btrdasd subvol sync`) |
| `backup` | `src/backup.rs` | ~3670 | `run_backup_job` (the one backup job for CLI and GUI), btrbk snapshot/send orchestration with volume deduplication, boot archival |
| `btrbk_conf` | `src/btrbk_conf.rs` | ~1030 | `btrbk.conf` renderer (shared by setup, `subvol` commands and sync); retired entries are not rendered |
| `caldate` | `src/caldate.rs` | ~270 | `YYYY-MM-DD` calendar-date arithmetic for adoption and retirement dates |
| `config` | `src/config.rs` | ~1810 | TOML config types, DAS/source/target models |
| `db` | `src/db.rs` | ~2580 | Database connection, schema, CRUD, FTS5 search, stats, pagination |
| `doctor` | `src/doctor.rs` | ~1070 | Subvolume drift detector (`btrdasd doctor --check-drift`) — compares configured subvolumes against what's actually on disk |
| `expire` | `src/expire.rs` | ~2350 | Expiry of retired subvolumes' backups per target and location, with its safety refusals (`btrdasd subvol expire`) |
| `fsutil` | `src/fsutil.rs` | ~170 | Atomic file replacement and the `CommandRunner` seam for host commands |
| `forget` | `src/forget.rs` | ~400 | Snapshot selection and deletion for `forget` / `purge`, with a live-series guard |
| `health` | `src/health.rs` | ~1860 | Drive health (SMART), mountpoint checks, serial→device resolution, scrub health, recovery OS lines |
| `indexer` | `src/indexer.rs` | ~610 | Snapshot discovery, span logic, walk orchestration |
| `maintenance` | `src/maintenance.rs` | ~870 | The DAS maintenance lock: `MaintenanceHeld`, the proof every target mount requires; holder records; the wait of `walk`, `restore` and the GUI's index and restore jobs (`--no-wait`, cancel) |
| `mount` | `src/mount.rs` | ~3640 | Source and target mounting by `mount_uuid` or serial, RAII `MountGuard`, unmount retry, `verify_write_targets()` |
| `progress` | `src/progress.rs` | ~610 | Progress reporting trait and `OrderedProgress`, the per-job ordered event queue (the D-Bus signal sink itself is in `btrdasd-helper`) |
| `reconcile` | `src/reconcile.rs` | ~400 | Drops index rows for snapshots no longer on disk (`btrdasd reconcile`), mountpoint-gated |
| `recovery_os` | `src/recovery_os.rs` | ~4570 | Read-only inspection of the independent OS on each mirror target, its staleness verdict against the host, the `RECOVERY OS` report section and the record `btrdasd health` and `scripts/recovery-os-vm.sh` read, each drive's naming the filesystem it was read from (`btrdasd recovery-os`) |
| `recovery_os::boot` | `src/recovery_os/boot.rs` | ~10900 | Whether booting a recovery OS would run btrbk, and when: the units its boot starts, what they run (shell scripts included), its cron, and the btrbk config each would use, down to one verdict (`will`, `may`, `no`) |
| `recovery_os::hold_disk` | `src/recovery_os/hold_disk.rs` | ~760 | `btrdasd recovery-os hold-disk`: the `O_EXCL` claim on a whole recovery disk that keeps the host from mounting it while `scripts/recovery-os-vm.sh` has lent it to the `recovery-os-updater` VM; held until SIGTERM/SIGINT/SIGHUP |
| `report` | `src/report.rs` | ~770 | Backup report formatting |
| `restore` | `src/restore.rs` | ~1700 | File and snapshot restore via btrfs send/receive, gated by `[restore] allowed_roots` and an unoverridable denylist |
| `scanner` | `src/scanner.rs` | ~135 | walkdir-based filesystem traversal |
| `schedule` | `src/schedule.rs` | ~430 | systemd timer management (show/set/enable/disable) |
| `scrub` | `src/scrub.rs` | ~3200 | Scheduled BTRFS scrub engine — locking, target resolution, pass tracking, exit-code split |
| `subvol` | `src/subvol.rs` | ~215 | Subvolume CRUD operations |
| `main` | `src/main.rs` | ~3750 | CLI entry point with clap subcommands |
| `setup/mod` | `src/setup/mod.rs` | — | Setup subcommand routing and root check |
| `setup/config` | `src/setup/config.rs` | — | Re-export of the library's `config` types |
| `setup/env_export` | `src/setup/env_export.rs` | — | `btrdasd config dump-env`: config as shell `DAS_*` variables for the scripts |
| `setup/detect` | `src/setup/detect.rs` | — | System detection (devices, init, packages) |
| `setup/templates` | `src/setup/templates.rs` | — | Render systemd units, cron, the udisks-ignore udev rule; embed the scripts (btrbk.conf comes from `btrbk_conf`) |
| `setup/installer` | `src/setup/installer.rs` | — | Install/uninstall/upgrade/check with manifest |
| `setup/retired_units` | `src/setup/retired_units.rs` | — | `setup --upgrade`'s one-time removal of the backup units older versions' `cmake --install` left under `/usr` (bd 7rf), recognised against `src/setup/retired_units/` — every byte but the install prefix in a service's `ExecStart=` — and kept when a link is above it or it was replaced while checked |
| `setup/wizard` | `src/setup/wizard.rs` | — | 9-step interactive dialoguer wizard |
| `btrdasd-helper` | `src/bin/btrdasd-helper.rs` | ~1920 | D-Bus daemon (feature `dbus`): 20 methods, 3 signals, polkit checks, job ownership |

### KDE Plasma GUI (`gui/src/`)

20 C++ components implementing full backup management (the previous read-only `Database`
QSqlDatabase wrapper was removed when the GUI's models were rewired to go through
`DBusClient`/`btrdasd-helper` exclusively — see `dbusclient.h/cpp` below):

| Component | Files | Purpose |
|-----------|-------|---------|
| MainWindow | `mainwindow.h/cpp` | KXmlGuiWindow with sidebar + QStackedWidget, rich status bar, keyboard shortcuts |
| Sidebar | `sidebar.h/cpp` | QTreeWidget navigation (Browse, Backup, Config, Health sections) |
| DBusClient | `dbusclient.h/cpp` | QDBusInterface wrapper; async method calls, JobProgress/JobLog/JobFinished signals |
| ProgressPanel | `progresspanel.h/cpp` | QDockWidget with progress bar, throughput, ETA, cancel, resizable raw log (native dock resize), smart auto-scroll |
| SnapshotModel | `snapshotmodel.h/cpp` | QAbstractItemModel tree (date groups → snapshots) |
| FileModel | `filemodel.h/cpp` | QAbstractTableModel with paginated loading (10k per page via D-Bus) |
| SearchModel | `searchmodel.h/cpp` | QAbstractTableModel for FTS5 search results |
| SnapshotBrowser | `snapshotbrowser.h/cpp` | Dolphin-style file browser; breadcrumb nav, detail/icon views, context menu, filter bar |
| BackupPanel | `backuppanel.h/cpp` | Mode selection, operation checkboxes, source/target selection, dry-run support |
| BackupSteps | `backupsteps.h/cpp` | The five operation ticks (snapshot, send, boot archive, index, email) as the `a{sv}` steps dictionary BackupRun carries |
| PanelConfig | `panelconfig.h/cpp` | Pure reader of the labels and `[boot] enabled` the Backup panel needs from config.toml (trailing comments tolerated); unit-tested in `gui-smoketest` |
| BackupHistoryView | `backuphistory.h/cpp` | QTableView of backup runs; auto-refresh on JobFinished |
| HealthDashboard | `healthdashboard.h/cpp` | Tabbed widget: Drives (D-Bus), Growth (chart), Status (timers/mounts) |
| ConfigDialog | `configdialog.h/cpp` | KPageDialog TOML editor with reload/diff/save toolbar |
| SetupWizard | `setupwizard.h/cpp` | QWizard first-run wizard: Welcome, Sources, Targets, Schedule, Summary |
| SnapshotTimeline | `snapshottimeline.h/cpp` | Custom QPainter widget for visual snapshot navigation |
| IndexRunner | `indexrunner.h/cpp` | D-Bus IndexWalk trigger (was QProcess, now D-Bus) |
| SnapshotWatcher | `snapshotwatcher.h/cpp` | QFileSystemWatcher with 30s debounce |
| RestoreAction | `restoreaction.h/cpp` | KIO::copy with file dialog destination |
| SettingsDialog | `settingsdialog.h/cpp` | KConfigDialog for paths and preferences |

### Tests

Counts from `cargo test --features dbus -- --list` and the `gui-smoketest` ctest case at 0.7.23.0
(2026-10-06); re-run that command rather than trusting these numbers.

| Suite | Count | Framework |
|-------|-------|-----------|
| Rust unit tests | 1130 | `#[cfg(test)]` modules in lib crate (`indexer/src/lib.rs`'s 23 `pub mod`s) |
| Rust CLI + setup tests | 263 | `#[cfg(test)]` modules in `main.rs` and `setup/` (`btrdasd` binary, not part of the lib crate) |
| D-Bus helper tests | 13 | `#[cfg(test)]` module in `src/bin/btrdasd-helper.rs` (built only with `--features dbus`) |
| Rust integration tests | 38 | `indexer/tests/integration_test.rs` (9), `subvol_cli.rs` (15), `recovery_os_cli.rs` (7), `record_run_contract.rs` (6), `setup_requires_root.rs` (1) |
| Rust loopback tests (manual, root-gated) | 8 | `indexer/tests/scrub_loopback.rs` (3), `subvol_sync_loopback.rs` (3), `boot_archive_loopback.rs` (2) — `#[ignore]`d; real loop-device BTRFS, not run by plain `cargo test` |
| C++ GUI smoke tests | 22 | `gui/tests/smoketest.cpp` (QTest, `QT_QPA_PLATFORM=offscreen`, built with `BUILD_TESTING`): formatting, D-Bus error mapping, panels constructing without a helper. The click-simulation suites were removed 2026-03-01 |
| **Total** | **1444 Rust (1452 incl. manual) + 22 Qt** | |
