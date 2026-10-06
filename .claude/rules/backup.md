# Backup System Rules

This file holds the binding rules only. The reasoning, incidents, measurements and
reproduction commands behind every section live under the **same heading** in
`.claude/docs/rules-reference/backup.md`, which is NOT auto-loaded.
**Read that section before changing anything a section here governs.**

## btrbk
- btrbk handles all BTRFS snapshot creation and send/receive. Never modify btrbk internals — use its CLI and config.
- Config at `/etc/btrbk/btrbk.conf` (canonical, generated from `config.toml`; never hand-edit).

## Never Run `setup --upgrade` or `cmake --install` While a Backup Is Running
Bash reads a running script as it goes. `setup` renames a new file into place, and the three
scripts end with `main "$@"; exit $?` (once `main` runs, bash never reads its file again), so a
running script is never read from a rewritten file. `cmake --install` (CMake 4.4.3, measured)
unlinks each file and creates a new one, executable once complete: a run already going keeps its
script but calls the new sibling scripts and `btrdasd` later. A run *starting* in that instant
mostly fails loudly (203/EXEC or 127), but bash reopens the script by path after the exec and can
find the new file still empty: **it exits 0 having done nothing** (measured: see the reference).
**`setup` refuses by itself**:
every mode that writes or removes installed files takes `/run/das-backup.lock`, then
`/run/das-maintenance.lock`, non-blocking, before its first write, holds both to the end, and exits
75 (on stderr) having changed nothing if either is held. **`cmake --install` takes no lock** —
check first, every time (it cannot see a run started by hand in the sub-millisecond before its flock):

```bash
busy=; for u in das-backup.service das-backup-full.service; do case "$(systemctl show -P ActiveState "$u")" in inactive | failed) ;; *) busy=1 ;; esac; done; flock -n /run/das-backup.lock true || busy=1; [ -z "$busy" ] || echo "WAIT — do not install"
```

Edit `config.toml`/`btrbk.conf` between runs: sync rewrites `btrbk.conf` before btrbk starts, and re-reads `config.toml`.

## Nested Subvolumes Need Their Own Config Entry — The Run Writes It
- **`btrfs send` does not descend into nested subvolumes.** A parent's snapshot holds an empty
  directory where a child subvolume sits, and the run reports success. Every subvolume still
  needs its own entry — **the backup run writes it** (`btrdasd subvol sync`: `backup-run.sh` calls
  it after verifying the sources and again re-verifies them; `btrdasd backup run` and the GUI
  helper call it too).
- Left out only by `[subvolumes].exclude` (pattern names the subvolume **without a trailing slash**
  and covers everything nested under it; `@tmp`, `@var-tmp` built in; `[doctor].exclude` still
  merged) or a `.snapshots` / `.btrbk-snapshots` path component. Every exclusion is in the run
  report (snapshot trees counted).
- A volume is read only if every device its sources name verifies (mountpoint, UUID, top level);
  otherwise nothing on it is adopted or retired and the run is marked failed.
- A gone subvolume's entry is **retired**, not an error: it leaves `btrbk.conf`. `subvol expire`
  (after btrbk) deletes its snapshots per target once retirement + that target's longest
  retention window has passed. No retention, unmounted, unreadable, shared directory,
  unrecognised names, source unmounted, name back, snapshot newer than retirement, clock < 2026, sync
  failed: kept and reported. The last config entry and an entry sending nowhere stay.
- A **Missing** or **Stale** finding from the weekly drift check now means sync itself failed.
- **Target scoping is part of the entry.** Bulk data gets `target_labels = ["primary-22tb"]`;
  only system-recovery data goes to the two 2 TB recovery drives. `target_labels = []` fans out
  to all three targets — right for `@`/`@home`/`@opt`, wrong for a growing VM image.
  **Accepted exception (2026-10-02):** `ISOs` and `SteamLibrary-local` stay in `hdd-system` and
  reach the recovery drives — small and static; reasoning in the reference. Not a precedent.
- A bulk payload inside a broadly-scoped subvolume defeats scoping invisibly (a directory has no
  entry to scope). Give it its own subvolume: `chattr +C` **before** any file lands, and copy
  with `cp --reflink=never`.
- When measuring btrbk's exit status, never read `$?` after a pipeline — it is the last
  command's. Use `${PIPESTATUS[0]}` and a positive control.
- Verify a mechanism actually fired (`backup_runs`) before describing it in the past tense.

## DAS Enclosure (Author's Setup)
- TerraMaster D6-320, 6-bay USB 3.2 Gen2 JBOD. **Gen2 is the rating; check the negotiated
  rate**: `cat /sys/bus/usb/devices/*/speed` must read `10000` (Mbit/s, link speed). It sat at
  `480` for nine days undetected. **Throughput does not reveal this** — only the link speed does.
- Bay map: `docs/examples/author-bay-mapping.md`; generic guide `docs/DAS-BAY-MAPPING.md`.
- **Targets are hidden from udisks** by the generated `/etc/udev/rules.d/99-das-backup-udisks-ignore.rules`
  (rendered from `[[target]]` serials + `mount_uuid`; never hand-edit). `btrdasd setup --check` reads
  back whether each attached target carries the flag. Nothing may mount them under `/run/media`.
- Targets are mounted only by this project's jobs, by `mount_uuid`, never by fstab. The 22 TB
  primary is BTRFS RAID-1 across two drives; the two 2 TB recovery drives are independent single-device filesystems.

## Targets and Retention (per `/etc/das-backup/config.toml`)
- **`primary-22tb`** — label `das-backup-22tb`, uuid `b2dbe07d-40b9-422e-8ccf-ef4931c40457`,
  serials `ZXA1R71M` (bay 2) + `ZXA1NYGZ` (bay 5), mount `/mnt/backup-22tb`. Retention
  `daily=7, weekly=4, monthly=12, yearly=1`. Receives every source stream; sole target for bulk ones.
- **`system-recovery-A-2tb`** — bay 1, `ZK208Q77`, label `das-backup-system-recovery-A`, mount
  `/mnt/backup-system-recovery-A`. Retention `daily=7`.
- **`system-recovery-B-2tb`** — bay 4, `ZFL41DNY`, label `das-backup-system-recovery-B`, mount
  `/mnt/backup-system-recovery-B`. Retention `daily=7`.
- The recovery drives are independent bootable copies, **not** RAID mirrors of each other.
- Their OS under `@` is read-only checked; stale, or btrbk running at its boot, is WARN.
- Boot archives: 60-day retention, pruned by `boot-archive-cleanup.sh` at the end of every
  `backup-run.sh` run (daily and full; not CLI/GUI runs) while targets are still mounted.

## Mount Options for the RAID-1 Primary Target
- `[das].mount_opts` includes `degraded`, so one failed leg does not block backup or restore.
- Data written while degraded lands as `single` chunks. After replacing the leg: `btrfs scrub`,
  then `btrfs balance start -dconvert=raid1 -mconvert=raid1`.
- `das-backup-22tb` is not in `/etc/fstab`; boot is unaffected by a leg failure.

## Boot Subvolume Archival
- Snapshot live `@`/`@home` read-only to `@.archive.<TS>` / `@home.archive.<TS>`, **then**
  replace the live subvolume from the latest btrbk snapshot. If the archive fails, the
  replacement is skipped — the only copy of the outgoing subvolume is never destroyed.
- Two code paths, which must stay symmetric: bash `update_boot_subvolumes()` in
  `scripts/backup-run.sh` (`--full` runs) and Rust `archive_boot()` in `indexer/src/backup.rs`.
  **Both locate the replacement BEFORE deleting anything**, and skip if none is found.
- **Snapshot names are read from `/etc/btrbk/btrbk.conf`, never re-derived**
  (`forget::live_subvol_snapshot_names()`). If it cannot be read, decline the whole step.
- **Both paths, and the pruner, skip `role=mirror` targets entirely** — the recovery drives carry
  their own independent OS under `@`/`@home`. Ordinary btrbk send/receive to them is unaffected.
- The pruner deletes only `@.archive.*` / `@home.archive.*` past retention on non-mirror
  targets. Old archives already on the mirrors are left for manual review, never auto-removed.

### Delete vs mutate — send/receive chain safety
- Deleting a *target* snapshot is harmless while one common pair survives (source UUID == target Received UUID).
- **NEVER clear `ro` on a received subvolume** — it permanently destroys the Received UUID.
- Mutating an already-sent LOCAL parent silently desynchronises source and target.
- In-place purging of a file is safe only in snapshots that are neither send sources nor received.

## Restore Destination Policy (since 0.7.20.0)
Checked before any directory is created, both roots compared **after resolution**:
- **`[restore] allowed_roots`** in `config.toml` — default `/home`, `/tmp`; `/srv/VirtualMachines`
  granted 2026-09-01. Widen it there, not in code, and grant a subdirectory, never `/srv` itself.
- **`RESTORE_DENIED_ROOTS`** (`indexer/src/config.rs`) is checked first and **no configuration can
  override it** — system paths plus the served roots `/srv/http` and `/srv/ftp`.
- **`O_NOFOLLOW` on every write.** Member paths that are absolute or non-literal are rejected.

## Email Reports
- **THIS PROJECT HOLDS NO MAIL CREDENTIAL and must never acquire one.** Reports go
  unauthenticated, plain, over loopback to the local relay at `127.0.0.1:25`, which authenticates
  upstream by **envelope sender**. Never copy a Resend key into any project or system config;
  never point automated mail at Protonmail Bridge or Proton submission.
- Both consumers read `[email]` from `config.toml`: `send_report()` in `scripts/backup-run.sh`
  and `send_email_report_with_kind()` in `indexer/src/report.rs`. `[email].from` selects the
  relay credential, so it must be `das-backup@thebosco.club`. `DAS_REPORT_TO` /
  `DAS_REPORT_FROM` override for testing.
- **`smtp-auth=none` is REQUIRED on every mailx invocation**, or s-nail aborts with exit 4.
- **Never redirect mailx stderr to `/dev/null`.**
- **Every mailx send is bounded** (`timeout -k 10 60`) and runs with the lock fds closed: s-nail
  gives up after ~45 s of silence by itself, but not on a relay that keeps trickling bytes, and
  such a relay must cost the report, not the run.
- The report is written to `$LAST_REPORT` before any send; a relay outage costs delivery only. A
  write that fails is logged as such, and the not-emailed lines then name the journal instead.
  With email off, an unsavable report is a FAIL (exit 3, `report:` in the row): the journal has
  the only copy.
- **Unattended (no-session) delivery is proven in production — do not re-test it.**
- Diagnose with `journalctl -u das-backup`, `journalctl -u postfix`, `mailq`. `status=sent` means
  the provider accepted it, not that it reached the inbox.

## Content Indexer
- Database `/var/lib/das-backup/backup-index.db`; span-based storage, FTS5 search, incremental.
- Soft-fail: indexing errors do not abort the backup, but must still reach the report.

## Scrub Ownership Boundary — exclusive in both directions (user decision 2026-08-02)
- **This project is the ONLY thing that scrubs the three `[scrub].targets` DAS filesystems.**
  Never add system-scope `btrfs-scrub@` timers for DAS media, from this or any other project.
- **This project never scrubs or services non-backup filesystems.**
- DAS targets are scrubbed sequentially in `[scrub].targets` order (shared USB path).
- `[scrub].on_calendar = *-*-01 03:05:00` is a *preference* for backup-first, not a guarantee;
  the backup timer has a 30-minute randomised delay. **Both orders are correct** — the interlock
  below makes the second job wait. An overrunning scrub defers the next backup, never skips it.

## Sentinel Interaction — `cachyos-sentinel` Auto-Restarts Failed Services
- `sentinel-core` restarts any unit it sees `failed`, including `das-backup.service` and
  `das-scrub.service`. **To stop either deliberately: `systemctl stop` AND `systemctl mask`** —
  stop alone is undone within seconds.
- The unit files carry no `Restart=`, `OnFailure=` or `OnUnitInactiveSec=`. Keep it that way.
- Sentinel's limiter is 3 restarts per 600 s and **cannot brake a failure loop slower than
  ~10 minutes**, so a failure the next start would meet again must never leave a unit `failed`:
  - `btrdasd scrub run`: **0** = the pass executed, whatever it found; **nonzero** = it could not
    start. Findings travel by email.
  - `backup-run.sh` (bd `d1r`): **0** = nothing FAILED (a WARN still 0); **3** = began its work and
    something FAILED, or it aborted on a target's or a source's state; **1** = could not start
    (nothing mounted or sent); **128+N** = stopped by HUP INT USR1 PIPE ALRM or TERM (129 130
    138 141 142 143), the unit fails. Both backup units carry `SuccessExitStatus=3`, and that line is load-bearing.
  - A 3 is not silent: the report says FAILURES DETECTED (for a FAIL recorded after the report is
    built — email delivery, the history record, a report saved nowhere — the row says it), and an
    abort before the report sends one **ABORTED** report (what, why, targets seen, log) and records
    a failed history row (bd `2my`).
    The one exception: an abort *after* the report went out exits 3 under a report and a row that
    already say what they saw — the journal's `status=3` and the log are its only trace.
  - `btrdasd backup run` (CLI, GUI; not run by the units; bd `vzsu`): the same 0 / 3 / 1 — see
    "The CLI/GUI Run Records Truthfully" below.
  - `btrdasd doctor`: **0** clean or deferred, **1** drift found, **2** could not run, **3** some
    volume failed to mount/list/unmount (outranks 1). `das-backup-doctor.service` carries
    `SuccessExitStatus=1`, and that line is load-bearing.
- Sentinel matches unit names by exact string — no globs. Prefer single non-template units.

## The CLI/GUI Backup Touches Only What Was Selected, And Mounts Nothing Itself (bd `7tx`)
- `btrdasd backup run|snapshot|send` and the GUI hand btrbk **only** the selected sources and targets,
  as filter arguments, one per (subvolume, target) pair: `<target dir>/<snapshot_name>`
  (`btrbk_conf::declared_pairs`, `backup::btrbk_filters`). Never a volume path (it cannot tell two
  sources on one volume apart) and never a source filter next to a target filter (btrbk's filters
  are a union, and a matched subvolume keeps every target).
- A label that is not in the configuration, or a selection that leaves nothing, **refuses** the step.
  An empty filter list is btrbk's "everything": it is passed only when the selection IS everything.
- **`backup snapshot` and `backup send` (CLI and helper) sync first, as `run` does**
  (`backup::sync_for_manual_step`, sources mounted, before the targets are). A selection of everything
  passes btrbk no filter, so it trusts `btrbk.conf`; sync brings that file into line with `config.toml`.
  Unlike `run`, a failed sync stops the step: `run` goes on because configured subvolumes must still be
  backed up, a step that cannot tell whether `btrbk.conf` is current has no such obligation.
- The run mounts **nothing** itself. `mount::ensure_sources_mounted` and `ensure_targets_mounted` mount,
  and the `MountGuard` each returns records what it mounted and gives exactly that back; `run_backup`
  only checks that each selected source's volume is a mount point and refuses if not (bd `7tx`, `8cf`).
- **An empty selection is a refusal, never "all"** (bd `7tx`). `BackupOptions.sources`/`.targets` are
  `Option`s: `None` = not specified (CLI with no flag: all), `Some(vec![])` = nothing ticked, refused by
  `backup::empty_selection` before the first lock or mount. The D-Bus `as` arguments cannot say "not
  specified", so the helper always passes `Some(list)` and refuses an empty `BackupSnapshot`/`BackupSend`
  list itself. A label (source or target) the configuration lacks is refused too, before the first lock
  (`backup::unknown_label`, exit 1), even beside known ones — never dropped, never widened to "all mounted".
- An unticked target is not read, so absent it cannot fail the step; a ticked one btrbk cannot read
  still fails it (exit 10). Never re-render `btrbk.conf` per run to get this: the retention baseline
  is the first primary target, so a reduced config renders the other targets' retention differently.

## The CLI/GUI Run Records Truthfully, And Exits By The Doctor's Rule (bd `no4`, `vzsu`)
- `BackupResult.snapshots_created`/`.snapshots_sent` are `Option<usize>`: **`None` = unknown** — the step
  that counts them was asked for and failed. Never `Some(0)` (a measurement: "nothing to do"). Stored as
  NULL (schema 4), printed `unknown` (summary, `backup run`), `null` (`--json`); GUI history shows "unknown".
- **`btrdasd backup run` exits 0 / 3 / 1 — the script's and the doctor's rule** (bd `vzsu`;
  `BackupJobOutcome::exit_code`): **0** clean (a warning too) or declined; **3** began and something
  failed, or aborted on a target's/source's state (`Aborted`: no target mounts, verification refuses,
  an absent ticked target — recorded as a failed row, counts NULL, unless a dry run); **1** could not
  start (`CouldNotStart`: empty selection, unknown label, locks). A report neither saved nor mailed fails the run
  BEFORE the row is written, so the row says `report: …`; a failed email beside a saved report stays a
  warning. The GUI has no exit codes: it shows `JobFinished(success, summary)` — exit 0 = success, 3 and
  1 = failure with the summary or reason.
- A failed `host.record` fails the job (`history not recorded: …` in `errors`), never only a warning: a run
  missing from the history that reports success is the fail-silent defect.

## Bare-Mountpoint Guard — REQUIRED in `backup-run.sh`
**Never invoke `btrbk` against a target path that is not a real mountpoint backed by the
expected DAS filesystem** — the write falls through to the NVMe root and fills it (`bd 9on`).
Two layers, both unconditional and both run under `--dryrun`:
1. **`create_mount_points`** — creates `$mnt` only for available targets. A stale bare directory
   for an unavailable one is `rmdir`'d if empty, and is a fatal ABORT if non-empty.
2. **`verify_targets_before_btrbk`** — after mounting, before `run_btrbk`: an available target
   must be a real mountpoint whose UUID (or serial) matches `config.toml`; an unavailable
   target's `$mnt` must not exist. Any violation aborts, exit 3, with an ABORTED report and a
   failed history row (bd `2my`).

Rust twin (CLI/GUI): `mount::verify_write_targets`, called inside every step that writes under a
target — `run_backup`, `send_snapshots`, `run_full_pipeline`, `archive_boot` (bd `7tx`) — so a caller
cannot skip it: `backup send` and `backup boot-archive` are covered from the CLI and from the helper.
`archive_boot` verifies the non-mirror targets whose mount point exists (the script's two safe
states: a real mountpoint, or absent); a bare directory refuses the whole step.
A target with no `mount_uuid` is identified by its drive's serial, as the script does (`findmnt` →
`lsblk` → `smartctl -i`); a serial that cannot be read is a refusal, never a pass (bd `7tx` 4).

## Maintenance Interlock — backup vs. scrub mutual exclusion
Backup, scrub, `reconcile` and `doctor` all mount and unmount the same filesystems and must never overlap.
1. **Singleton lock**, non-blocking: `/run/das-{backup,scrub,reconcile,doctor}.lock`. Held ⇒ skip;
   a backup lock that cannot be opened or taken is "could not start", exit 1 (bd `ismb`).
2. **Maintenance lock**: `/run/das-maintenance.lock`, shared by all sides and held for the whole
   operation. Backup and scrub **wait** for it, never skipped; `reconcile` and `doctor` defer.
- **Every target mount needs it** — `mount::ensure_targets_mounted` takes a `MaintenanceHeld` (bd `frb`); `walk`/`restore` (CLI, GUI) wait, `--no-wait` exits 75; a holder that runs them hands its hold down (`DAS_MAINTENANCE_LOCK_FD`) or they fail.

**Always acquire in that order — singleton, then maintenance — never the reverse.** That
ordering alone is what makes the pair deadlock-free; release order is irrelevant.
