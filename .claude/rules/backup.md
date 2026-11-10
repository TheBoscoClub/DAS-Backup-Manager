# Backup System Rules

Binding rules only. Detail, incidents, mechanism and measurements per section: `.claude/docs/rules-reference/backup.md`
(same headings, NOT auto-loaded) — **read that section before changing anything a section here governs.**
Installation specifics: `.claude/rules/local-installation.md` (gitignored, this machine only).

## btrbk

- btrbk does all BTRFS snapshot creation and send/receive. Use its CLI and config; never modify its internals.
- Config `/etc/btrbk/btrbk.conf` (canonical, generated from `config.toml`; never hand-edit).

## Never Run `setup --upgrade` or `cmake --install` While a Backup Is Running

**`setup` refuses by itself**: every mode that writes or removes installed files takes `/run/das-backup.lock`
then `/run/das-maintenance.lock`, non-blocking, before its first write, holds both to the end, and exits 75 (on stderr), changing nothing, if either is held.
**`cmake --install` takes no lock** — check first, every time:

```bash
busy=; for u in das-backup.service das-backup-full.service; do case "$(systemctl show -P ActiveState "$u")" in inactive | failed) ;; *) busy=1 ;; esac; done; sudo flock -n /run/das-backup.lock true || busy=1; [ -z "$busy" ] || echo "WAIT — do not install"
```

Edit `config.toml`/`btrbk.conf` between runs: sync rewrites `btrbk.conf` before btrbk starts, and re-reads `config.toml`.

## Nested Subvolumes Need Their Own Config Entry — The Run Writes It

- **`btrfs send` does not descend into nested subvolumes** (the run reports success on an empty directory):
  every subvolume needs its own entry — **the run writes it** (`btrdasd subvol sync`: `backup-run.sh` calls it between
  verifying and re-verifying sources; `btrdasd backup run` and the GUI helper call it too).
- Omitted only by `[subvolumes].exclude` (pattern names the subvolume **without trailing slash**, covers
  everything nested under it; `@tmp`, `@var-tmp` built in; `[doctor].exclude` still merged) or a `.snapshots` / `.btrbk-snapshots` path
  component. Every exclusion is in the run report (snapshot trees counted). A pattern never skips a configured entry.
- **Each source volume's top level (id 5) is a subvolume too** (bd `xtnq`): entry `.`, primary-only,
  named `<volume basename>-toplevel`, never retired, excluded only by the exact pattern `/`. Never live
  in a source reaching a mirror (validation and the renderer both refuse). To stop its backup: delete
  the entry AND exclude `/`.
- A volume is read only if every device its sources name verifies (mountpoint, UUID, top level);
  otherwise nothing on it is adopted or retired and the run is marked failed.
- A gone subvolume's entry is **retired**, not an error: it leaves `btrbk.conf`. `subvol expire` (after
  btrbk) deletes its snapshots per target only after retirement + that target's longest retention window;
  anything doubtful (no retention, unmounted/unreadable target, shared directory, name back, snapshot newer
  than retirement, failed sync) is kept and reported. The last config entry and an entry sending nowhere stay.
- A **Missing** or **Stale** finding from the weekly drift check means sync itself failed.
- **Target scoping is part of the entry.** Bulk data names its bulk target (`target_labels = ["<bulk target>"]`);
  only system-recovery data goes to the small recovery drives. `target_labels = []` fans out to every
  target — right for `@`/`@home`/`@opt`, wrong for a growing VM image. Any exception is a recorded decision, not a precedent.
- A bulk payload inside a broadly-scoped subvolume defeats scoping invisibly: give it its own subvolume
  (`chattr +C` **before** any file lands; `cp --reflink=never`).
- Verify a mechanism actually fired (`backup_runs`) before describing it in the past tense.

## Subvolume Coverage

**[scoped]** `backup-coverage.md` — the `(volume, subvolume, target)` coverage check.

## DAS Enclosure

- **Check the negotiated USB link rate, not just throughput**: `cat /sys/bus/usb/devices/*/speed` must read the
  enclosure's rated speed (`10000` Mbit/s for USB 3.2 Gen2); a downgrade to `480` is invisible to a backup that
  still completes. Bay-map guide: `docs/DAS-BAY-MAPPING.md`.
- **Targets are hidden from udisks** by generated `/etc/udev/rules.d/99-das-backup-udisks-ignore.rules`
  (never hand-edit; `btrdasd setup --check` reads the flag back). Nothing may mount them under `/run/media`.
- Targets are mounted only by this project's jobs, by `mount_uuid`, never by fstab. A RAID-1 primary spans
  two drives; small recovery drives are independent single-device filesystems.

## Targets and Retention (per `/etc/das-backup/config.toml`)

- Targets, labels, serials, mounts and retention are configuration, never code defaults (installation
  values: `local-installation.md`).
- Recovery drives (`role = "mirror"`) are independent bootable copies, **not** RAID mirrors of each other. Their OS
  under `@` is read-only checked (stale, or btrbk running at its boot, is WARN); rebuilt by
  `recovery-os-vm.sh provision`; the host never writes to the drive. Its ESP, named by `esp_partuuid`, is
  mounted read-only for the reading and each missing loader-entry file is a BOOT PROBLEM (bd `sp78.20`).
- Boot archives: 60-day retention by default (`[boot].archive_retention_days`), pruned by `boot-archive-cleanup.sh`
  at the end of every `backup-run.sh` run (not CLI/GUI runs) while targets are still mounted.

## Mount Options for a RAID-1 Primary Target

- `[das].mount_opts` includes `degraded`, so one failed leg does not block backup or restore.
- Data written degraded lands as `single` chunks. After replacing the leg: `btrfs scrub`, then
  `btrfs balance start -dconvert=raid1 -mconvert=raid1`.
- A backup target is never in `/etc/fstab`; boot is unaffected by a leg failure.

## Boot Subvolume Archival

**[scoped]** `backup-boot-archive.md` — archiving and replacing `@`/`@home`, the pruner.

### Delete vs mutate — send/receive chain safety

- Deleting a *target* snapshot is harmless while one common pair survives (source UUID == target Received UUID).
- **NEVER clear `ro` on a received subvolume** — it permanently destroys the Received UUID.
- Mutating an already-sent LOCAL parent silently desynchronises source and target.
- In-place purging of a file is safe only in snapshots that are neither send sources nor received.

## Restore Destination Policy (since 0.7.20.0)

**[scoped]** `backup-restore.md` — `allowed_roots`, denied roots, the one followed link; **read it before granting a restore root on the host**.

## Email Reports

**[scoped]** `backup-email.md` — relay, `report::Mailer`, `smtp-auth=none`, bounded sends; **read it before diagnosing a missing report**.

## Content Indexer

- Database `/var/lib/das-backup/backup-index.db`; span-based storage, FTS5 search, incremental.
- Soft-fail: indexing errors do not abort the backup, but must still reach the report.
- **The state subvolume (`/var/lib/das-backup`, `after_index = true`) is sent after indexing** by `run_state_pass` from `/etc/btrbk/btrbk-state.conf`, never by the main btrbk run. A `State index pass` WARN means the targets' index copy is a night old; a recovery OS reads it at boot through `das-recovery-state-sync`.

## Scrub Ownership Boundary — exclusive in both directions

- **This project is the ONLY thing that scrubs the `[scrub].targets` filesystems**, and **never scrubs or
  services non-backup filesystems.** No other component adds a scrub timer for the backup media.
- Targets are scrubbed sequentially in `[scrub].targets` order. `[scrub].on_calendar` is a *preference* for
  backup-first, not a guarantee: **both orders are correct** — the interlock makes the second job wait; an
  overrunning scrub defers the next backup, never skips it.

## Supervisor Interaction — A Service Supervisor May Restart Failed Units

- A supervisor (systemd `Restart=`, a monitor that restarts any unit it sees `failed`) must never meet a
  failure the next start would meet again (it would loop). **To stop a unit deliberately: brake it, then stop
  it** — `/run/systemd/system/<unit>.d/brake.conf` with `[Unit]` `RefuseManualStart=yes`, `daemon-reload`, then
  `systemctl stop` the unit and its timers; release: remove the drop-in, `daemon-reload`, start the timers (a
  reboot clears both). Never `systemctl mask`: the units live in `/etc/systemd/system`, so it is refused, and
  `--runtime` is outranked (bd `8f8j`).
- The unit files carry no `Restart=`, `OnFailure=` or `OnUnitInactiveSec=`. Keep it that way: a failure the
  next start would meet again must never leave a unit `failed`. Prefer single non-template units.
  - `btrdasd scrub run`: **0** = the pass executed, whatever it found (findings travel by email); **nonzero** = could not start.
  - `backup-run.sh` (bd `d1r`): **0** = nothing FAILED (a WARN still 0); **3** = began its work and
    something FAILED, or aborted on a target's or source's state; **1** = could not start (nothing mounted or sent); **128+N** =
    stopped by a signal (129 130 138 141 142 143), the unit fails. Both backup units carry
    `SuccessExitStatus=3`; that line is load-bearing. Coverage changes none of these.
  - A 3 is not silent: the report says FAILURES DETECTED (a FAIL recorded after the report is built is named in
    the history row), and an abort before the report sends one **ABORTED** report and records a failed row
    (bd `2my`); an abort *after* the report went out leaves only the journal's `status=3` and the log.
  - `btrdasd backup run` (CLI, GUI; not run by the units; bd `vzsu`): same 0 / 3 / 1 — see "The CLI/GUI Run Records Truthfully" below.
  - `btrdasd backup boot-archive` (CLI): **0** clean or declined (a backup holds the lock), **3** the
    step, the unmount after it, or the mount once its locks were held failed, **1** only when it could not
    start (config, locks).
  - `btrdasd doctor`: **0** clean or deferred, **1** drift found (an undelivered pair included, bd `sp78.7`),
    **2** could not run, **3** some volume or target failed to mount/list/unmount (outranks 1). `das-backup-doctor.service` carries
    `SuccessExitStatus=1`; that line is load-bearing.
  - `das-recovery-os-update-*.service` carries `SuccessExitStatus=1 3 4 5 6 7`; that line is load-bearing.
    A refused, warned or failed session is not a failed unit (outcome: `recovery-os-vm-history.jsonl`),
    never a restart. **A supervisor is never the tool that retries a recovery-drive session** (bd `01ne`).
    `recovery-os-vm.sh session` refuses to be the main process of a unit without that line;
    `session ... --detach` is the way to run one in the background.

## The CLI/GUI Backup Touches Only What Was Selected, And Mounts Nothing Itself (bd `7tx`)

**[scoped]** `backup-cli-gui.md` — selection-only btrbk filters, no mounting, truthful counts and exit codes.

## Bare-Mountpoint Guard — REQUIRED in `backup-run.sh`

**Never invoke `btrbk` against a target path that is not a real mountpoint backed by the expected DAS
filesystem** — the write falls through to the NVMe root and fills it (`bd 9on`). Two layers, both
unconditional, both run under `--dryrun`:

1. **`create_mount_points`** — creates `$mnt` only for available targets. A stale bare directory for an
   unavailable one is `rmdir`'d if empty, a fatal ABORT if non-empty.
2. **`verify_targets_before_btrbk`** — after mounting, before `run_btrbk`: an available target must be a
   real mountpoint whose UUID (or serial) matches `config.toml`; an unavailable target's `$mnt` must not
   exist. Any violation aborts, exit 3, with an ABORTED report and a failed history row (bd `2my`).

Rust twin (CLI/GUI): `mount::verify_write_targets`, called inside every step that writes under a target
(`run_backup`, `send_snapshots`, `run_full_pipeline`, `archive_boot`; bd `7tx`) so no caller can skip it (the D-Bus helper exposes only `BackupRun`; `backup send` and
`backup boot-archive` reach the guard from the CLI).
`archive_boot` verifies the non-mirror targets whose mount point exists; a bare directory refuses the
whole step. A target with no `mount_uuid` is identified by its drive's serial, as the script does (`findmnt` → `lsblk` → `smartctl -i`); an unreadable serial is a
refusal, never a pass.

## Maintenance Interlock — backup vs. scrub mutual exclusion

Backup, scrub, `reconcile` and `doctor` all mount and unmount the same filesystems; never overlap.

1. **Singleton lock**, non-blocking: `/run/das-{backup,scrub,reconcile,doctor}.lock`. Held ⇒ skip; a
   backup lock that cannot be opened or taken is "could not start", exit 1 (bd `ismb`).
   - Every holder of `/run/das-backup.lock` writes `<who> pid <pid>` into it and empties it (never removes it) before release: `backup-run.sh`, `btrdasd backup …`, `btrdasd recovery-os record`, `btrdasd setup …`, `config edit` (`backup::SingletonHeld`, bd `al9`).
   - Open the singleton `<>`, never `>` (a run that does not get the lock must not empty the holder's record).
   - `backup-run.sh` finds it held by setup or a config edit → waits, `SINGLETON_WAIT_SECS` (900 s), saying whom; still held → skip, exit 0, a WARN that this backup did NOT run.
   - Held by a backup → skip, exit 0, naming it. No record, no pid, or the pid gone → skip, exit 0, with the same WARN; never a silent skip.
2. **Maintenance lock**: `/run/das-maintenance.lock`, shared by all sides, held for the whole operation.
   Backup and scrub **wait** for it, never skipped; `reconcile` and `doctor` defer.

- **Every target mount needs it** — `mount::ensure_targets_mounted` takes a `MaintenanceHeld` (bd `frb`);
  `walk`/`restore` (CLI, GUI) wait, `--no-wait` exits 75; a holder that runs them hands its hold down
  (`DAS_MAINTENANCE_LOCK_FD`) or they fail.
- **A target left mounted by an earlier job is taken down by the next** (bd `jgm`, operator 2026-11-09):
  shown to be the target (UUID, else serial), it is unmounted — 5 tries, then one `umount -f` — and
  mounted afresh; only if that fails too does the job refuse (`backup-run.sh`: exit 3, ABORTED). A
  foreign filesystem at the path refuses, never unmounted. Never `umount -l`. A handed-down hold uses it as found.

**Always acquire singleton, then maintenance — never the reverse.** That ordering alone makes the pair
deadlock-free; release order is irrelevant.
