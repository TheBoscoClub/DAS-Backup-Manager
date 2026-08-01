# Scheduled Scrub Integration — Analysis & Proposal

**Status**: **IMPLEMENTED** (Option B) — bd tasks `ikn`, `212`, `0kn`, `atq`, `5kb`, `b6f`, landed
2026-08-01. This document is kept as the historical design record; it is no longer the source of
truth for current behavior. See `ARCHITECTURE.md`'s "Scrub Pipeline" section for how the shipped
system actually works (engine, state file, health integration, systemd units).
**Written**: 2026-07-27
**Origin**: investigated from the `CachyOS-Kernel` project while setting up host-wide scrub timers.
That project deliberately stopped at the DAS boundary — the non-DAS filesystems now have scrub
timers, and everything touching TerraMaster drives was left here, where it belongs.

---

## 1. Why this exists

The host now has monthly BTRFS scrub timers for every **non-DAS** filesystem:

| Instance | Filesystem | Profile | Duration |
|---|---|---|---|
| `btrfs-scrub@hddRaid1` | `/hddRaid1` (2 × ST24000DM001, SATA) | RAID1 | ~18 h 35 m |
| `btrfs-scrub@dasRaid0` | `/dasRaid0` (4 × ST2000DM008, **SATA** despite the name) | RAID0 data / RAID1C3 meta | ~1 h |
| `btrfs-scrub@srv` | `sata_pool` (2 × SATA SSD) | RAID0 data / RAID1 meta | ~15–20 m |
| `btrfs-scrub@-` | `/` (2 × NVMe) | RAID1 | ~15–30 m |

They use the stock Arch template `/usr/lib/systemd/system/btrfs-scrub@.timer`
(`OnCalendar=monthly`, `RandomizedDelaySec=1w`, `Persistent=true`; service runs `Nice=19`,
`IOSchedulingClass=idle`).

**The three DAS filesystems cannot use that template**, because they are unmounted by design between
backup runs — a calendar timer would fire and find no filesystem. That is the problem to solve here.

A dangling `btrfs-scrub@backups.timer` (enabled 2025-10-13, targeting `/backups`, a path that does
not exist) was disabled on 2026-07-27. It had never protected anything.

## 2. Current scrub state of the DAS filesystems

Read from `/var/lib/btrfs/scrub.status.<fsuuid>` (persists across unmounts, which is what makes
resume viable):

| Filesystem | UUID | Last scrub | Bytes scrubbed | Errors |
|---|---|---|---|---|
| `das-backup-22tb` | `b2dbe07d…` | **2026-05-24 23:04** (64 d) | 5.40 TiB | read/csum/verify/uncorrectable/corrected all **0** |
| `das-backup-system-recovery-A` | `60b05268…` | 2026-05-24 13:54 (64 d) | **0 bytes** ⚠️ | all 0 |
| `das-backup-system-recovery-B` | `7c7ae72d…` | **2026-04-17 09:30** (101 d) | 904 GiB | all 0 |

Two things to note:

- The 22 TB array's scrub is the one launched manually in the 2026-05-24 session (see
  `.claude-checkpoint-notes.md`) — it completed cleanly. So the array *is* verified, just not on a
  schedule, and not for 64 days.
- **`recovery-A` reports `finished:1` with `data_bytes_scrubbed:0`.** A completed scrub that
  verified nothing, on a filesystem holding 1.02 TiB. Worth investigating separately — it means
  recovery-A has effectively never been integrity-checked. Likely candidates: the scrub was started
  against a just-mounted/not-ready filesystem, or cancelled instantly and still recorded as finished.

## 3. What the codebase actually supports today

Searched `*.sh`, `*.rs`, `*.toml`, `*.service`, `*.timer`, `*.md`:

- **Scrub appears only in documentation** (`STORAGE-ARCHITECTURE-AND-RECOVERY.md`,
  `DISASTER-RECOVERY-GUIDE.md`, `EMERGENCY-QUICK-REFERENCE.md`, bay-mapping docs). There is **zero
  scrub logic in code**. Nothing to extend — this is new capability.
- `btrdasd` subcommands are `walk, search, list, info, setup, config, backup, restore, schedule,
  subvol, health, completions`. **No `scrub`.** There is an obvious, currently-empty home here.
- `backup-run.sh` has **no hook/plugin mechanism** (`grep -E "hook|post_run|pre_run|plugin"` → nothing).

### `backup-run.sh` main() flow — the mount window

```
check_root → check_das_connected → set_io_scheduler → create_mount_points
→ mount_sources → create_snapshot_dirs → mount_targets      ← targets mounted
→ create_target_dirs → verify_targets_before_btrbk
→ [capture_usage] → run_btrbk → [indexer, report, db record]
→ unmount_all                                               ← targets unmounted
→ "Backup complete. DAS can be safely disconnected."
```

The window between `mount_targets` and `unmount_all` is the only time the DAS filesystems are
mounted under this project's control.

## 4. THE TRAP — read this before designing anything

`unmount_all()` unmounts like this:

```bash
umount "${ALL_TARGET_MOUNTS[$i]}" 2>/dev/null || true
```

**Failures are silently discarded.** A running scrub holds the filesystem busy, so `umount` returns
`EBUSY`, the loop swallows it, and the script then logs *"All volumes unmounted"* and *"Backup
complete. DAS can be safely disconnected."* — both false, with the array still mounted.

**Any design must guarantee the scrub is finished or cancelled before `unmount_all` runs.** Because
scrub state persists in `/var/lib/btrfs/`, cancelling is cheap and fully resumable — `btrfs scrub
cancel` then `btrfs scrub resume` later loses no progress. Time-boxing is therefore safe.

## 5. Hard constraint: the config files are generated

Both carry a "do not edit" header:

```
/etc/das-backup/config.toml   # Generated by btrdasd setup — do not edit.
/etc/btrbk/btrbk.conf         # Generated by btrdasd setup — do not edit.
```

Any hand-added `[scrub]` keys are destroyed by `sudo btrdasd setup --upgrade`. **Config-driven scrub
requires changes to the Rust `setup` code that emits these files.** This is the single biggest
constraint on the design.

## 6. Options

### Option A — standalone monthly unit, no project code changes

A dedicated service + timer that mounts the DAS targets by UUID (reading `config.toml` read-only),
scrubs under a `RuntimeMaxSec` time box, and **always** `btrfs scrub cancel` + `umount` in
`ExecStopPost`. Guarded with `Conflicts=das-backup.service das-backup-full.service` and `After=` so
it can never overlap a backup run.

- ✅ Survives `btrdasd setup --upgrade`; systemd guarantees the unmount even on timeout or crash;
  removable in one command; closes the gap immediately.
- ⚠️ Mount logic lives outside the project — mild tension with the single-canonical-source rule.

### Option B — build it into the project (the proper home)

1. `[scrub]` section emitted by `btrdasd setup` (e.g. `enabled`, `interval`, `max_duration`,
   `targets`).
2. A time-boxed scrub phase in `backup-run.sh` between `run_btrbk` and `unmount_all` — the array is
   *already mounted*, so no second spin-up and no duplicated mount logic.
3. A `btrdasd scrub` subcommand for manual start/status/cancel.
4. Surface last-scrub age and error counts in `btrdasd health` (and therefore the GUI).

- ✅ Architecturally correct, config-driven, upgrade-safe by design, visible in health/GUI.
- ⚠️ Real code change + release cycle (repo has CI and CodeFactor).

### Option C — extend the existing mount-triggered resume unit — **NOT recommended**

`btrfs-scrub-resume-das-22tb.service` is `WantedBy=run-media-bosco-das\x2dbackup\x2d22tb{,1}.mount`.
Adding `mnt-backup\x2d22tb.mount` would make it fire at the *start* of a backup, competing with btrbk
for the same USB bus, and then `unmount_all` fails silently per §4. Also note this unit currently
only knows the **udisks** mountpoints, so it has never participated in a backup run at all.

## 7. Time budget

| Filesystem | Data | Scrub reads | Estimate |
|---|---|---|---|
| `das-backup-22tb` | 4.84 TiB, RAID1 | ~9.7 TiB (both copies) | ~7–10 h over USB |
| `recovery-A` | 1.02 TiB, single device | ~1.02 TiB | ~1.5–2 h |
| `recovery-B` | 1.02 TiB, single device | ~1.02 TiB | ~1.5–2 h |

The May 2026 run measured ~340 MiB/s on the 22 TB array, consistent with the upper end being ~8 h.

Two shapes to choose between:
- **One unbounded monthly pass** — finishes in a single night (host never sleeps), simplest.
- **Bounded and resumable** — e.g. 3 h per run, completing a full pass over ~3 sessions. Bounded
  impact, more moving parts, exercises the resume path routinely.

Note the recovery disks are **single-device**: scrub *detects* corruption but cannot repair it (no
second copy). Still worth doing — it names the damaged file so it can be re-copied from the primary.

## 8. Recommendation

**Option A now, Option B as the destination.** A closes a 64–101 day gap without touching a project
whose config is machine-generated; its script then becomes the reference implementation for B.

## 9. Open questions for the discussion

1. One long monthly pass, or bounded-and-resumable?
2. Should scrub run after **every** backup (time-boxed) or on its own monthly schedule?
3. Should a scrub finding errors trigger the existing email report path (`parse_pbridge_smtp` /
   `send_report`), or a separate alert?
4. Should `btrdasd health` fail/warn when last-scrub age exceeds a threshold?
5. Separately: why did `recovery-A` record a finished scrub with 0 bytes scrubbed?

## 10. Cross-references

- Host-wide scrub facts and the safety rationale (why scrub is safe on mirrored profiles, why the
  RAID5/6 warnings do not apply, SSD endurance non-issue): `~/.claude/rules/infrastructure.md`,
  section "BTRFS scrub schedule".
- Canonical storage topology (label + UUID + device count for every filesystem):
  same file, section "Storage Topology — CANONICAL".
- This project remains authoritative for bay mapping (`docs/examples/author-bay-mapping.md`) and
  backup array topology/history (`.claude/rules/backup.md`).
