---
paths:
  - "indexer/src/raid_guard.rs"
  - "indexer/src/mount.rs"
  - "indexer/src/maintenance.rs"
  - "indexer/src/scrub.rs"
  - "indexer/src/doctor.rs"
  - "indexer/src/backup.rs"
  - "indexer/src/main.rs"
  - "indexer/src/report.rs"
  - "indexer/src/db.rs"
  - "indexer/src/bin/btrdasd-helper.rs"
  - "indexer/tests/**"
  - "gui/src/raidacceptdialog.*"
  - "gui/src/healthdashboard.*"
  - "gui/src/dbusclient.*"
  - "polkit/**"
  - "scripts/backup-run.sh"
  - "scripts/backup-verify.sh"
  - "tests/test_backup_exit_semantics.sh"
---

# Backup System Rules — RAID-1 Primary Target Guard

Detail per section: `.claude/docs/rules-reference/backup.md` (same headings, NOT auto-loaded) — **read that section before changing anything this file governs.** Split out of `backup.md` 2026-11-23 (instruction-size limit, moved verbatim); loads only when a matching file is read or edited.
The lines a hand command needs (members, the only mount path, `raid accept`'s typed label, lifting a rejection, mount options, scrub then balance, fstab) stay in `backup.md` §Mount Options for a RAID-1 Primary Target.

## Mount Options for a RAID-1 Primary Target

- A member is a filesystem UUID AND a configured serial (operator 2026-11-23, bd `su1b.27`): each partition `blkid` lists is read for its drive's serial (partition, parent disk, udev `ID_SERIAL_SHORT`; `MemberHost::member_serial`, `health::serial_of_partition`) before it is scanned, and every member line (OK, DEGRADED, READ-ONLY, STALE-READ-ONLY, raid accept's listing, a refusal) names `serial <S>` beside devid, dev_uuid and path.
- A partition carrying the filesystem on a drive whose serial is not in `[[target]].serials`, or whose serial cannot be read → the target is refused for every intent, naming partition, serial (or why unreadable) and dev_uuid, before any scan or mount; the way out → add the serial to config (a legitimate replacement) or detach/wipe the drive. A target with no configured serial has nothing to bind to and is not checked.
- A configured serial whose drive is attached but carries no member → a warning naming it; an absent configured drive is not unbound and still decides the degraded mount (`serial_attached`).
- The mount → without `degraded` first; a member not present, or a plain mount that fails → poll every 5 s for `[das].member_wait_secs` (900, at most 3600; operator 2026-11-22, bd `su1b.11`), rescanning.
- Degraded → only when, after the wait, a member is absent AND a configured serial is absent; every drive attached with a member unfound or a plain mount still failing → refused, never degraded.
- A degraded mount → a WARN row every run (operator 2026-11-22: it keeps backing up) and the period recorded first: primary `/var/lib/das-backup/raid/<fsid>.record` (`DAS_RAID_RECORD_DIR`), witness `<mount>/.das-backup-raid.record`, both written before the mount is used.
- Split brain → refused (exit 3, ABORTED): present members not all among the record's; either copy unreadable or corrupt → refused (only ENOENT is "no record").
- Every caller of `ensure_targets_mounted` / `mount_guarded_target` passes `raid_guard::Job::<job>.intent()`; never a bare `Intent`, never inferred from the mount options (`every_target_mount_names_its_job` scans for it).
- `Intent::ReadOnly` → only a job that persists nothing derived from the target's contents: restore file, restore snapshot, browse, the doctor's report.
- `Intent::Write` → everything else, including walk, the helper's index walk, reindex, reconcile and the recovery-OS reading: they write the index or a state file from what they read (security review 2026-11-22).
- Split brain with `Intent::ReadOnly` → mounted `<mount_opts>,ro,degraded,rescue=nologreplay`, a `STALE-READ-ONLY` WARN naming the date its data stops, no record change (operator 2026-11-22, bd `su1b.11`); an unreadable record stays refused for every job.
- `raid accept` shows → the present member (devid, dev_uuid, superblock generation), the period given up (since/last), the backup runs the history records inside it (`unknown`, never 0, when they cannot be counted).
- `raid accept` writes → the primary record, then the witness through a degraded read-write mount: a new degraded period on the present members, every member of the old period in `rejected=`; then one journal line (`logger -t btrdasd`) and one `raid_accepted` row (schema 9); a half-written accept is finished by running it again.
- A rejected member → refused for every job, alone or beside the others, before it is scanned or mounted (from the primary copy); found only in the witness → the normal mount taken straight down and refused.
- An unreadable primary record → every `Intent::Write` mount refused before anything is scanned; `Intent::ReadOnly` → only the read-only mount (`READ-ONLY`), nothing written (review F1).
- Every member present, no primary record, superblock generations differing → never a read-write mount: `Write` refused, `ReadOnly` the read-only mount; equal generations mount normally (review F1).
- Every member present, superblock generations differing, whatever the primary says → each member's own witness read alone first (the others `btrfs device scan --forget <exact path>`, the member mounted `ro,degraded,rescue=nologreplay` at a private directory, unmounted, the others scanned again on every path); a member any copy rejects → `Write` refused, `ReadOnly` only the non-rejected member read-only (review round 3).
- That per-member read → refused, nothing forgotten or mounted, while the filesystem is mounted anywhere; a probe that fails or an unreadable own copy → `Write` refused; equal generations → never forgotten or probed.
- `raid accept`'s second look → a listed member whose superblock cannot be read, or an absent drive's serial attached again → refused before any mount (the post-mount devinfo check is the backstop only).
- `raid accept` → looks at the members again after the label is typed (any other member present: refused, nothing written), then mounts, then reads devinfo: a member besides the accepted one present → unmounted at once, nothing written (review F2).
- Every member back → normal mount, the record becomes scrub-owed (a WARN row, a doctor finding) until a completed scrub begun after the rejoin clears it.
- Before a job unmounts a RAID-1 target it mounted read-write and not degraded → `raid_guard::check_before_unmount`: a member the kernel shows missing, or a view that cannot be read → a degraded period recorded on the members present (both copies) and a WARN (bd `su1b.14`).
- That end check runs in `MountGuard`'s unmount (and its `Drop`), before the scrub engine's own unmounts, and in `backup-run.sh`'s `unmount_all` through hidden `btrdasd backup raid-check` (fd 8 handed down; the target's row turns WARN).
- A mount that turns a degraded record into scrub-owed → status `REJOINED`; the job then starts `das-scrub.service` (`systemctl start --no-block`) only after its maintenance lock is released: Rust in `MaintenanceHeld`'s drop, the script in `start_owed_scrub` after closing fds 7, 8 and 9.
- The owed-scrub start → never from the scrub's own rejoin, never in a dry run, refused in a test build; a failed start is a WARN, never an error; the pass scrubs every `[scrub].targets` filesystem (no per-target run) and a backup preempts it by the claim.
- After any mount of a RAID-1 target → `/sys/fs/btrfs/<fsid>/devinfo/*/missing` unreadable is refused on both paths; a member missing is refused by `verify_targets_before_btrbk` unless the guard mounted it degraded this run, and by `verify_write_targets` while every configured drive is attached.
- A Rust run's report → one `RAID-1 <label>  WARN` row per record (`report::raid_rows`), the script's → the target's own row.
- A refusal → `MountError::RaidRefused`: it fails the job, `walk` and `restore` included (their split-brain case is the read-only mount above).
