#!/bin/bash
# backup-run.sh - Run btrbk backup to DAS drives (config-driven)
# Version: 4.11.3
# Date: 2026-10-05
#
# Features:
#   - A dry run's boot-archive cleanup is not a FAIL (v4.11.3):
#     run_archive_cleanup() requires a per-target summary of the pruner, and
#     looked only for the real run's, "Deleted N, kept N, errors N". The
#     pruner's dry run printed "Would keep N, found expired archives above",
#     so every dry run, the install script's included, logged "printed no
#     per-target summary", recorded archive_cleanup as FAILED and exited 3,
#     a false failure that trains the operator to ignore the line.
#     boot-archive-cleanup.sh 2.1.2 now prints "Would delete N, kept N, errors
#     N" in a dry run, one shape for both modes, and each mode here requires
#     its own verb: a real run that printed the dry run's, or a dry run that
#     printed the real run's, is still a FAIL, and so is a run with no summary
#     at all (bd DAS-Backup-Manager-zwr; tests/test_boot_archive_cleanup.sh).
#   - The boot-subvolume step tells "not mounted" from "could not tell"
#     (v4.11.3): update_boot_subvolumes() asks probe_mount_point, whose three
#     answers it used to fold into two. A target whose mountpoint check could
#     not run — a missing mountpoint program (exit 127), or no descriptor free
#     for its redirection — read as "not mounted": it was skipped, counted as
#     neither skipped nor failed, and the step recorded OK, 0 updated, 0
#     skipped, a green result for work that was not looked at. "Could not
#     tell" now fails the step: counted, said with the probe's message and the
#     target's name, and the targets after it are still done. "Not mounted",
#     and a path that is not there, stay a quiet skip (bd
#     DAS-Backup-Manager-jlsz; tests/test_early_exit_readers.sh).
#   - A pid is ASCII digits (v4.11.3): maintenance_holder() matches the pid in
#     the lock file's record with [[:digit:]]. Under en_US.UTF-8, the host's
#     locale, bash's regex [0-9] also matches digits of other scripts and
#     superscripts, so a record whose "pid" was written in Arabic-Indic digits
#     read as a pid whose process had gone, "no longer running", where it is a
#     record with no pid in it. The other [0-9] and [A-Za-z] regex matches in
#     the scripts got the same treatment (bd DAS-Backup-Manager-1bsx;
#     tests/test_maintenance_lock.sh).
#   - "DAS can be safely disconnected" only on knowledge (v4.11.2, with the
#     entry below): the target unmount gate asks probe_mount_point, which
#     tells "not mounted" from "could not tell". A mountpoint error on a
#     target used to read as "not mounted": the target was skipped, the report
#     said "Unmount targets OK", the run exited 0 and said the DAS could be
#     safely disconnected while a drive was still mounted — on a link going
#     bad, the operator might pull it on that word. "Could not tell" now
#     FAILS the gate whatever happens next (exit 3, FAILURES DETECTED), the
#     unmount is tried anyway, and the run says the DAS is NOT safe to
#     disconnect and why: the detail names each target not known to be
#     released — "still mounted: …", or "could not tell whether … is
#     mounted — <the probe's message>; umount then succeeded" (or "failed
#     too") — in the report row, the history row and the last line. An
#     absent drive's mount point, removed on purpose, answers mountpoint 1
#     "No such file or directory" like an error, and is not one: the gate
#     passes. The last line claims safe only on the gate's own OK, never by
#     default (bd DAS-Backup-Manager-jug6; tests/test_unmount_all.sh,
#     tests/test_backup_exit_semantics.sh).
#   - A run unmounts only the source volumes it mounted (v4.11.2): a run never
#     owns a mount it found in place. mount_sources() records each mount point
#     it mounts, once however many sources share it (nvme and nvme-vm share
#     /.btrfs-nvme), and unmount_all() unmounts those and nothing else, last
#     mounted first, each once, on every way out: the end of main(), and
#     cleanup() after an abort or a stop, dry runs alike. A source volume
#     already mounted when the run looks is used as found, and the log says
#     so; one fstab declares but the run finds unmounted is said too, as
#     INFO, since only the run's helper then stands in for fstab's mount. The
#     run used to unmount every source: since cb5937b turned the old cleanup
#     of its own helper mounts into "unmount every source", the first run
#     after a boot took down the fstab mounts /.btrfs-nvme, /.btrfs-ssd and
#     /.btrfs-hdd, down until the next boot (each later run mounting and
#     unmounting them itself), and every run tried /dasRaid0, the operator's
#     general-use filesystem. That failed every night with a WARN that could
#     not say why: umount's message went to /dev/null. The nested fstab mount
#     /dasRaid0/VirtualMachines pinned it, not running VMs as the comment
#     said: umount refuses a mount with another mounted beneath it (reproduced
#     on this kernel, 2026-10-04), and that night no VM ran and no process
#     held /dasRaid0. A helper mount that will not unmount is still a WARN,
#     not a FAIL and not part of the disconnect gate, now with umount's own
#     message. The probe before each unmount (probe_mount_point) tells "not
#     mounted" from "could not tell": mountpoint's error read as "not
#     mounted" struck the run's helper off its record and left it mounted,
#     with nothing said; now the run says it could not tell, with the probe's
#     message, and unmounts anyway (review F1). The record is written before
#     mount runs, and a test now holds it there: a stop while mount runs
#     still takes the mount down (review F2). Source verification is
#     unchanged: a source found mounted must still be the expected
#     filesystem at its top level, or the run aborts (3) — and now leaves the
#     mount it refused as it found it. The Rust path's MountGuard already
#     left a pre-existing source mount alone (bd DAS-Backup-Manager-8cf;
#     tests/test_unmount_all.sh, tests/test_backup_exit_semantics.sh).
#   - No external hostname program (v4.11.1): the report's Host: line (the
#     full report's and the ABORTED one's), the mail subject and the From
#     display name read bash's own $HOSTNAME, and the short name is
#     ${HOSTNAME%%.*}, everything before the first dot, as `hostname -s`
#     printed it. bash sets HOSTNAME from gethostname() when it starts
#     (measured under `env -i`; neither unit source sets Environment=), so
#     under set -u it is always set and needs no default.
#     The script ran the `hostname` program, inetutils on Arch, which no
#     packaging declares. Without it a run did not stop: it mailed a report
#     with no host name — "Host:" empty, a subject of "[DAS Backup]  —
#     SUCCESS", a From of "DAS Backup ()" — and "hostname: command not found"
#     in the journal, once for each call. main's CI container has none, and
#     the exit-semantics suite stopped at its harness check. That suite no
#     longer whitelists the program, so a run that reaches for it fails there,
#     and it checks the Host line, the subject and the From name. Behaviour is
#     otherwise unchanged (bd DAS-Backup-Manager-arv1;
#     tests/test_backup_exit_semantics.sh).
#   - Exit status 0 / 3 / 1 (v4.11.0), operator decision C of 2026-10-04,
#     the doctor's rule: 0 = the run executed and nothing FAILED (a WARN
#     still exits 0); 3 = the run began its work and something FAILED or it
#     aborted (btrbk nonzero for some or all targets, any FAIL operation, an
#     abort on a target's or a source's state); 1 = it could not start
#     (nothing mounted or sent). Both units list SuccessExitStatus=3, so
#     systemd and cachyos-sentinel see success and never restart the run,
#     while the journal still shows status=3. It used to exit 1 whenever
#     btrbk exited nonzero, and btrbk exits 10 when any ONE target aborts
#     (measured 2026-10-02, a run by hand with recovery drive A pulled:
#     btrbk 10, this script 1). Under the unit that is `failed` on every
#     run, and sentinel, which restarts a failed unit, would start a new
#     ~25-minute backup about every ten minutes until the drive came back.
#     Every exit path is in EXIT STATUS below. cleanup() turns any status
#     of a run that held the maintenance lock into 3, so an implicit set -e
#     abort (a source's `mount` failing with 32, say) cannot fall outside
#     the rule, and it runs with errexit off: a log line it could not write
#     used to end it with exit 1 and skip the unmount (bd
#     DAS-Backup-Manager-d1r; tests/test_backup_exit_semantics.sh).
#     An abort is not silent: a run that aborts with 3 before its report sent
#     no report and wrote no history row, so with the unit no longer failed
#     nothing at all showed it — a powered-off DAS would have failed every
#     night unseen. cleanup() now records it as failed (--counts-unknown, the
#     reason in --errors) and sends one ABORTED report through send_report:
#     what aborted, why, what was backed up, which targets were seen, whether
#     it is in the history, where the log is. Both best effort; neither
#     changes the status (bd DAS-Backup-Manager-2my). Only a singleton lock
#     another run holds is a skip (0): one that cannot be opened or taken is
#     "could not start" (1) and says why, where it used to skip silently
#     (bd DAS-Backup-Manager-ismb). The snapshot counters are decided before
#     the run status and the report, so a counter failure reads FAILURES
#     DETECTED in the report as in the history (bd DAS-Backup-Manager-bzw).
#     The boot-subvolume step's drift check is matched by bash itself,
#     `[[ =~ ]]`: piped into grep -q, a listing over 64 KiB read as "no
#     snapshots" when printf died of SIGPIPE (bd DAS-Backup-Manager-wkvz),
#     and as a here-string it needed a temp file, which a full /tmp or no
#     fd to spare turned into the same answer (round 4, N3). Its digits are
#     [[:digit:]], ASCII in every locale as grep's [0-9] was: bash's regex
#     [0-9] also matches non-ASCII digits under en_US.UTF-8 (round 5, N5).
#     Every report's delivery is bounded (MAIL_TIMEOUT_SECS, 60 s, then KILL
#     10 s later) and mailx runs without the lock fds. s-nail gives up by
#     itself after about 45 s of silence on a read, but not on a relay that
#     keeps trickling bytes, and that held the run, the DAS mounted and both
#     locks held, for as long as it trickled (d1r round 3, M1; the 45 s
#     measured in round 4, N2).
#     HUP, PIPE, USR1 and ALRM are trapped like INT and TERM: each keeps
#     its own code and is recorded as a stop by name, where they read as
#     "a command that failed" — exit 3, an ABORTED mail — and then killed
#     the run anyway. cleanup() ignores PIPE, so a stdout that went away
#     cannot end it before the unmount (round 3, M2). A last report that
#     cannot be written is said, never "Report saved", and each line that
#     says a report was not emailed names the journal when the file failed
#     too (round 3, M3). An abort names its step, never
#     unexpanded command text: a source that will not mount says which,
#     its device and mount's message; a set -e failure, the call chain it
#     failed in (round 3, M4).
#     The Snapshot counts row reads N/A until the counts are decided, like
#     every other row; decide_run_counts records OK with them (round 3, M6).
#     A source that mounts with a warning (util-linux's "source write-
#     protected, mounted read-only") logs it again, in the journal and the
#     log file: M4's capture of mount's output dropped it (round 4, N1).
#     With email off, a report that cannot be saved is a FAIL (exit 3, the
#     history row says why): the journal has the only copy, where it used
#     to end in exit 0 and a success row (round 4, N4).
#   - Recovery OS boot warning (v4.10.0): `btrdasd recovery-os status` now
#     also exits 1 for a current recovery OS whose boot may run btrbk —
#     something enabled there (a unit, its timer, or cron) runs btrbk and its
#     config is there, or that could not be told — with a WARNING row in its
#     block. check_recovery_os reads the section's Result and WARNING rows to
#     say why it records WARN: "stale", "btrbk may run at boot", both, or
#     "needs attention" when the section shows neither; an OK detail says
#     "nothing needs attention" (bd DAS-Backup-Manager-1yg).
#   - A rewrite of this file while main runs is never read (v4.9.2): the
#     last line is `main "$@"; exit $?`. Bash reads a script as it runs, so
#     a copy over this file in place — truncate and write the same inode, as
#     a plain `cp` does and `btrdasd setup` did — that landed while main ran
#     had bash read the NEW file at the old offset once main returned: a
#     tail, half a line (exit 127). That line is read whole before main
#     starts, and after main there is nothing left to read. A copy in place
#     that lands during bash's parse of this file, before main is called
#     (about 3 ms), can still end it there. `btrdasd setup` now renames a new
#     file over the old one. Behaviour is otherwise unchanged
#     (bd DAS-Backup-Manager-6wt; tests/test_script_rewrite.sh).
#   - A failed run is recorded, with unknown counts as unknown (v4.9.1):
#     record_run_args() builds the `btrdasd backup record-run` vector and
#     says an unknown snapshot count with --counts-unknown, which the history
#     stores as NULL. It used to send `--snaps-created -1`, which record-run's
#     parser refused as an unknown option, so every run whose counters could
#     not be read — the failed ones — was left out of the history, and the
#     history showed the previous success as the latest run. The counts are
#     unknown when `btrbk list latest` failed, when its output held no field
#     this parser knows, and when the run ended before capture_report_data()
#     read them (BTRBK_LATEST_RAW_OK now starts empty, not "true", so an
#     abort no longer records 0 created, 0 sent). A record that fails is a
#     FAIL (run_history), no longer a warning: report_unrecorded_run() writes
#     the report again — FAILURES DETECTED, and a RUN HISTORY section saying
#     the run is missing — and sends it again, since the first copy went out
#     before the record was attempted. Exit codes are unchanged
#     (bd DAS-Backup-Manager-6wt).
#   - Maintenance lock holder record and hand-down (v4.9.0): once it holds
#     /run/das-maintenance.lock this run writes "backup-run.sh pid <pid>"
#     into it (record_maintenance_holder), so a restore or index job that
#     finds the lock held can say what it waits for, and cleanup() empties
#     it again on the way out, while the lock is still held. The file is
#     opened `<>` instead of `>`, which emptied the current holder's record
#     while this run merely waited; a run that has to wait names the holder
#     (maintenance_holder) instead of guessing at a scrub. `btrdasd walk` now
#     takes the maintenance lock before it mounts the targets, so run_indexer
#     hands it the lock this run holds (DAS_MAINTENANCE_LOCK_FD=8) —
#     otherwise walk would wait for this run forever
#     (bd DAS-Backup-Manager-frb).
#   - Recovery OS check and a WARN level (v4.8.0): check_recovery_os() runs
#     `btrdasd recovery-os status` after btrbk and expiry, while the targets
#     are still mounted, and puts its RECOVERY OS section in the report. It
#     reads each role=mirror drive's own OS under @ (never writes to the
#     drive, not even an access time) and records the reading in
#     $DAS_RECOVERY_OS_STATE for `btrdasd health` (not in a dry run). The read
#     is bounded by `timeout` (RECOVERY_OS_TIMEOUT_SECS, 300 s): a stalled
#     drive costs a FAIL, never the run. A stale OS records WARN: the status line
#     reads COMPLETED WITH WARNINGS and the subject SUCCESS WITH WARNINGS, but
#     the run is recorded as a success, because the backup itself worked. A
#     check that cannot run records FAIL. Neither stops the run
#     (bd DAS-Backup-Manager-xd3).
#   - Report honesty (v4.7.1): a failed `btrbk list latest` shows as
#     "(unavailable: …)" in LATEST SNAPSHOTS, not "(none yet)".
#   - Dry run sees the planned btrbk.conf (v4.7.0): in --dryrun, sync renders
#     the btrbk.conf the real run would leave into a mktemp file
#     (`subvol sync --dry-run --render-btrbk-conf`), `btrbk dryrun` reads that
#     one, and the EXIT trap removes it. A pending retirement no longer makes
#     the dry run fail on an entry the real run retires first; config.toml
#     and btrbk.conf are untouched. create_target_dirs also creates the
#     target directory a pending adoption's new source sends to, as the real
#     run does after its reload, logging each one, and now runs after
#     verify_targets_before_btrbk (bd DAS-Backup-Manager-g17).
#   - Unmount retry (v4.6.1): unmount_all() tries each DAS target up to
#     UMOUNT_ATTEMPTS times, UMOUNT_RETRY_PAUSE seconds apart, before it
#     records the unmount as FAILED — a target is often busy for a moment
#     after btrbk (bd DAS-Backup-Manager-5oc). umount's own message is logged.
#   - Subvolume sync (v4.6.0): sync_subvolumes() adopts new and retires
#     vanished subvolumes before btrbk runs and reloads the config;
#     expire_retired_subvolumes() deletes retired series past their window
#     after btrbk. Neither can stop the backup; both are recorded in the report.
#   - Source-side mount verification (v4.5.0): verify_sources_before_write(),
#     called from main() between mount_sources() and create_snapshot_dirs().
#     Targets have had verify_targets_before_btrbk() since bd
#     DAS-Backup-Manager-9on; sources had only mount_sources()'s
#     `mountpoint -q`-and-mount, with nothing checking WHAT got mounted. An
#     empty /.btrfs-hdd/.btrbk-snapshots dated 2026-05-17 was found on the
#     NVMe root filesystem, i.e. create_snapshot_dirs() had already run once
#     against a bare mountpoint. The new guard requires every source volume to
#     be a real mountpoint AND to carry the filesystem UUID its config `device`
#     resolves to AND to be mounted at the top-level volume (FSROOT '/'). The
#     mountpoint check is load-bearing and not redundant with the UUID check:
#     for the `nvme` source the root filesystem IS the source filesystem, so
#     `findmnt --target` returns a MATCHING UUID for a bare /.btrfs-nvme.
#     Honest scope: a `device` given as a /dev path (live config: `nvme`,
#     `nvme-vm`) is resolved through blkid at verification time, which is a
#     consistency check, not an identity check — see the function header.
#     Tracks bd DAS-Backup-Manager-zlv.
#   - cleanup() guaranteed on every abort path (v4.4.1): 'trap cleanup ERR'
#     (installed inside main(), no 'set -E'/errtrace) only ever fired for a
#     failing command directly in main()'s own body — never for a set -e
#     abort or explicit `exit 1` inside a called function (create_mount_points,
#     verify_targets_before_btrbk, mount_targets, run_btrbk, check_root, ...).
#     Reproduced empirically on bash 5.3.15: `false`, `exit 1`, and
#     `x=$(false)` inside a helper function all killed the script with zero
#     cleanup() output — no unmount_all, no FAILURE DB row, DAS left mounted,
#     the run invisible to the GUI history. Replaced with a top-level
#     `trap cleanup EXIT`, which bash runs on every termination path
#     (normal completion, `exit N` at any call depth, a set -e abort at any
#     call depth). New SCRIPT_COMPLETED flag, set as the last statement of
#     main(), makes cleanup() a true no-op on the clean-completion path
#     (unmount_all/record_backup_run_in_db already ran in main()'s own
#     body in that case) while still running the abort-recovery body for
#     every earlier exit. `trap 'exit 130' INT`/`trap 'exit 143' TERM` route
#     signal deaths (e.g. `systemctl stop das-backup` mid-run sending TERM)
#     through the same EXIT trap instead of bash's default "die without
#     running EXIT traps" behavior for unhandled fatal signals — explicit
#     128+signum codes, not a bare `trap 'exit' ...`, which was proven in
#     the harness to reuse a stale pre-signal $? (often 0) and mask a
#     mid-run TERM abort as a false success. cleanup() now captures
#     `$?` as its first statement and finishes with `exit "$rc"` so the
#     original exit code — 0 for a clean run, nonzero for an abort — is
#     never clobbered by cleanup's own command exit statuses (unmount_all's
#     internal calls are already `if`-guarded/non-propagating; the
#     record_backup_run_in_db and unmount_all calls in cleanup() are also
#     `|| true`-guarded so a failure inside the abort-recovery body itself
#     cannot skip the trailing `exit "$rc"`).
#     CLEANUP_ARMED gate (same v4.4.1 change, added after review): the
#     EXIT trap above is installed, and ALL_TARGET_MOUNTS is populated,
#     entirely at top level — both well before main() is ever entered.
#     Review confirmed a literal "singleton-skip" collision (a second
#     backup-run.sh instance's flock-held skip triggering cleanup) does
#     NOT reproduce, because that skip's `exit 0` (top-level, before the
#     singleton-lock section) happens before the trap is even installed —
#     proven empirically by running a real flock-held second instance and
#     observing no cleanup() output. But a DIFFERENT, real exposure was
#     found: main()'s own argument-parsing usage error (`exit 1` for an
#     unrecognized flag) or a check_root() failure aborts AFTER the trap
#     is active but BEFORE this process holds /run/das-maintenance.lock —
#     at that point unmount_all() would run over ALL_TARGET_MOUNTS with no
#     exclusivity guarantee against indexer/src/scrub.rs, which mounts the
#     SAME /mnt/backup-* paths under that identical lock and states the
#     invariant this closes: "a backup can never unmount a filesystem out
#     from under a running scrub". New CLEANUP_ARMED flag, set "true" in
#     main() immediately after acquire_maintenance_lock() returns (the
#     earliest point this process actually owns the shared mountpoints);
#     cleanup()'s recovery body now requires
#     `CLEANUP_ARMED == "true" && SCRIPT_COMPLETED != "true"`, silently
#     preserving the original exit code otherwise. Tracks bd DAS-Backup-Manager-oeo.
#   - Report generated after unmount, from cached mount-time data (v4.4.0):
#     previously generate_report() ran BEFORE unmount_all(), so an unmount
#     failure was visible only in the console/journal — never in the same
#     run's email, and never reflected in the DB-recorded overall_status.
#     New capture_report_data() snapshots df -h / df --output=avail capacity
#     figures and `btrbk list latest` per target while targets are still
#     mounted, into CAPACITY_USED/CAPACITY_AVAIL/CAPACITY_PCT/AVAIL_BYTES,
#     an ordered REPORT_TARGETS array, and BTRBK_LATEST. generate_
#     capacity_section()/generate_growth_section() now iterate
#     REPORT_TARGETS and read only these caches — no live mountpoint/df
#     calls — so report output is byte-identical to the pre-unmount case.
#     main()'s real-backup path is reordered to run_archive_cleanup ->
#     capture_report_data -> unmount_all -> overall_status -> generate_
#     report -> send_report -> record_backup_run_in_db, and BACKUP
#     OPERATIONS gained a dedicated "Unmount targets" row fed by
#     OP_STATUS[unmount]/[unmount_detail]. overall_status now includes
#     unmount failures — intentional: the email subject and DB row now
#     correctly flip to FAILURE when the DAS didn't unmount cleanly. Tracks
#     bd DAS-Backup-Manager-ecg.
#   - Boot subvolume snapshot finder pattern fix (v4.3.1): update_boot_subvolumes()'s
#     latest_root/latest_home finders were matching zero snapshots on every
#     run, on every target, silently. btrbk.conf's nvme volume sets
#     snapshot_name "root-" for @ (and "home" for @home), so on-disk
#     snapshot names are "root-.<TS>" / "home.<TS>" — the finder's old
#     `nvme/root\.` pattern (root immediately followed by a dot) has NEVER
#     matched "root-.<TS>" (an extra "-" sits between "root" and the dot).
#     Pre-existing since the snapshot_name scheme was introduced, predates
#     this file's v4.x history entirely — found 2026-08-01 during the real
#     das-backup-full.service proof run, whose journal showed
#     "[das-backup-22tb] No btrbk snapshots found, skipping" even though
#     btrbk had just created fresh snapshots that same run. Combined with
#     the correct role=mirror skip on the two recovery targets, this made
#     the scheduled full-backup boot-subvolume-refresh path a complete
#     no-op on every target, every time it ran on a schedule — only manual
#     Rust-path runs (indexer/src/backup.rs) ever actually updated @/@home.
#     Fixed by anchoring both patterns to the real snapshot name (including
#     the "-" for root) and the real timestamp shape (btrbk's default
#     `timestamp_format long` is YYYYMMDD<T>HHMM — 4-digit HHMM, NOT
#     HHMMSS — with an optional "_N" collision suffix), verified live
#     against the mounted 22TB target's actual subvolume list. Tracks
#     DAS-Backup-Manager-ycr; bd DAS-Backup-Manager-01u (the config-vs-script
#     drift detector) is the systemic guard against this class of bug for a
#     future rename.
#   - Maintenance interlock + honest unmount reporting (v4.3.0):
#     * Backup side of the mutual-hold interlock shared with the scrub engine
#       (indexer/src/scrub.rs): after the existing fd-9 singleton lock on
#       /run/das-backup.lock succeeds, acquire_maintenance_lock() takes a
#       BLOCKING flock on /run/das-maintenance.lock (fd 8) before any mount
#       work, held to process exit. A backup whose scheduled time lands
#       mid-scrub defers and starts the moment the scrub releases the lock —
#       never skipped, never canceled. A non-trivial wait logs a visible
#       "waiting" line and a "lock_wait" OP_STATUS entry with the wait
#       duration. Lock path and singleton->maintenance ACQUISITION order
#       match scrub.rs exactly, which is what makes the pair deadlock-free
#       by construction — release order does not need to match (this side
#       just holds both fds open to process exit; the scrub engine's
#       ScrubLocks additionally releases maintenance before singleton via
#       Rust field-drop order, but that symmetry isn't required here).
#       Tracks DAS-Backup-Manager-b6f.
#     * unmount_all() no longer discards DAS target umount failures via
#       `|| true`. Every configured target mountpoint (ALL_TARGET_MOUNTS) is
#       attempted (a stuck unmount no longer masks the rest); a path that
#       isn't currently mounted is skipped via `mountpoint -q` rather than
#       counted as a failure. Failures are collected and reported via
#       record_op "unmount" FAIL, and the final "DAS can be safely
#       disconnected" message is now conditional on every target having
#       actually unmounted — previously it printed unconditionally even when
#       a target was still mounted. Source volumes remain a best-effort,
#       untracked unmount since they are host filesystems outside the
#       physical DAS enclosure and some (das-storage on /dasRaid0) can
#       legitimately stay busy indefinitely (running VMs) — a source unmount
#       failure now logs a visible log_warn (previously fully silent via
#       `|| true`) but still never touches record_op or the disconnect gate.
#       Tracks DAS-Backup-Manager-b6f.
#     * run_btrbk()'s dryrun branch is now guarded the same way the real-run
#       branch always was — a `btrbk dryrun` failure records
#       record_op "btrbk" FAIL and lets the script continue (soft-fail)
#       instead of aborting under set -euo pipefail with no report entry.
#       Tracks DAS-Backup-Manager-vuw.
#   - Boot archive-then-recreate + pruner wiring (v4.2.4): update_boot_subvolumes()
#     now archives the outgoing @/@home as a read-only snapshot
#     (@.archive.<TS> / @home.archive.<TS>, TS=YYYYMMDDTHHMMSS) BEFORE the
#     create-then-swap replacement on --full runs, matching indexer/src/backup.rs
#     archive_boot() semantics exactly. If the archive snapshot fails, the
#     recreation is skipped for that subvolume so the only copy is never
#     destroyed. boot-archive-cleanup.sh (previously installed but never
#     invoked) is now run at the end of every backup, daily and full alike,
#     while targets are still mounted, and reports through
#     record_op "archive_cleanup". Tracks DAS-Backup-Manager-64h, -1j7.
#   - Incremental BTRFS backups via btrbk to configured targets
#   - Maintains stable boot subvolumes (@ and @home) for disaster recovery
#   - Detects DAS drives by serial number (stable across reboots)
#   - RAID-1 multi-serial + mount-by-UUID: surviving-leg backups continue when
#     a single RAID-1 partner is missing (degraded mount), no manual config
#     edit required. See DAS-Backup-Manager-2qe.
#   - Bare-mountpoint guard (v4.2.2): refuses to invoke btrbk unless every
#     configured target either (a) is a real mountpoint backed by the
#     expected UUID/serial, or (b) does not exist on disk at all. Unmounted
#     bare directories at $mnt are auto-removed (when empty) so btrbk
#     cannot silently write to / instead of the DAS. Prevents recurrence of
#     the May 2026 incident where a removed 22TB drive let btrbk fill the
#     root filesystem.
#   - Snapshot_dir absolute-path resolution (v4.2.3): create_snapshot_dirs
#     now composes ${SOURCE_VOLUMES[label]}/${SOURCE_SNAPSHOT_DIRS[label]}
#     before mkdir, so newly-added sources whose snapshot_dir is relative
#     (the common case in config.toml) land on the source's volume rather
#     than systemd's cwd (/). Fixes the silent-skip pattern where btrbk
#     would emit "Failed to fetch subvolume detail for snapshot_dir" for
#     7 consecutive nightly runs after the 2026-05-17 commit added the
#     hdd-media and hdd-system sources. Tracks DAS-Backup-Manager-0p2.
#   - Logs per-target throughput (data written + MB/s rate)
#   - Designed for unattended nightly execution
#   - All configuration loaded from config.toml via btrdasd
#   - Single-instance guard via flock on /run/das-backup.lock — second
#     invocation exits 0 immediately if another is running. Protects against
#     timer fires during long runs, Sentinel auto-restart races, and direct
#     manual invocations.
#
# Prerequisites:
#   - DAS connected and powered on
#   - btrdasd built and installed
#   - config.toml installed at /etc/das-backup/config.toml
#
# Usage:
#   sudo ./backup-run.sh              # Incremental backup
#   sudo ./backup-run.sh --dryrun     # Preview only
#   sudo ./backup-run.sh --full       # Force full backup (recreate boot subvols)
#
# EXIT STATUS (bd DAS-Backup-Manager-d1r, operator decision C, 2026-10-04)
#
#   0  the run executed and nothing FAILED. A WARN (a stale recovery OS:
#      COMPLETED WITH WARNINGS) still exits 0.
#   3  the run began its work — it holds the maintenance lock — and something
#      FAILED, or it aborted. Whatever caused it will usually still be there
#      ten minutes later (an absent drive, a target that will not mount), so
#      both units list SuccessExitStatus=3: systemd and cachyos-sentinel see
#      success and do not restart a whole backup for it. The journal still
#      shows status=3, the report says FAILURES DETECTED and the history row
#      says failed. A run that aborts before its report sends one ABORTED
#      report instead and is recorded as failed all the same (cleanup(),
#      bd DAS-Backup-Manager-2my) — a dry run sends and records nothing.
#   1  it could not start: nothing was mounted or sent. A unit that fails
#      this way fails in seconds, which sentinel's 3-per-600 s limiter does
#      brake.
#
# Every exit path, and why it is what it is:
#
#   status   where                         when
#   0        top level, singleton lock     another backup holds /run/das-backup.lock
#                                          (`flock -E 75` answers 75): a skip, not a failure
#                                          (before the EXIT trap; touches nothing)
#   1        top level, `exec 9>`          the lock file cannot be opened (not root, /run
#                                          missing): says so (bd DAS-Backup-Manager-ismb)
#   1        top level, flock              the lock cannot be taken for any other reason
#                                          (ENOLCK: 71; a flock that answers 1 to anything):
#                                          says so, with flock's status (ismb)
#   1        top level                     btrdasd missing: a required tool
#   1        top level, load_config_env    the config cannot be read
#   1        top level, set -u             the config lacks a value this script reads
#   1        main, argument parsing        an unknown argument (usage)
#   1        main, check_root              not root
#   1        main, before the lock         the log directory or file, or the maintenance lock
#                                          file, unusable — any set -e abort before the
#                                          maintenance lock is held (cleanup: abort_exit_status)
#   3        check_das_connected           no primary target available: a target's state
#   3        create_mount_points           an absent target still mounted that will not
#                                          unmount; the bare-mountpoint guard (an absent
#                                          target's directory is not empty)
#   3        mount_sources                 a source volume that will not mount: names the source,
#                                          its device and mount's message (round 3, M4)
#   3        verify_sources_before_write   a source volume is not the expected filesystem
#                                          (called twice: before and after subvolume sync)
#   3        verify_targets_before_btrbk   a target is not mounted (it failed to mount) or
#                                          holds the wrong filesystem, or an absent target's
#                                          directory exists
#   3        anywhere once the lock is     any other command failing under set -e (a target
#            held                          directory that cannot be made, say): the run began
#                                          its work and stopped (cleanup: abort_exit_status)
#   0        end of main                   no operation FAILED (completed_exit_status)
#   3        end of main                   any operation FAILED: btrbk nonzero (10 when one
#                                          target aborts, 1/2 when btrbk itself fails), an
#                                          absent recovery target, subvolume sync or expiry,
#                                          the recovery OS check (not a stale OS: that is a
#                                          WARN), boot subvolumes, archive cleanup, unmount,
#                                          indexer, USB link speed, email delivery, the
#                                          history record, the snapshot counters, a report
#                                          saved nowhere (email off, the file unwritable)
#   129 130 138 141 142 143
#            wherever the run is           SIGHUP SIGINT SIGUSR1 SIGPIPE SIGALRM SIGTERM
#                                          (`systemctl stop` sends TERM): the signal's own
#                                          code, kept — a stop is not a run's outcome, and a
#                                          stopped unit ends failed (stop it with mask, too).
#                                          A status that only looks like one — a pipeline's
#                                          SIGPIPE 141, a child killed by TERM 143 — is a
#                                          command that failed: 3 or 1, as above (STOP_SIGNAL
#                                          tells them apart; round 3, M2)
#
# Every 3 above that ends before main()'s report — all but the end-of-main
# row — is recorded as failed and sends one ABORTED report naming what
# aborted (abort_reason; a set -e failure by the call chain it failed in), unless it
# is a dry run (bd DAS-Backup-Manager-2my). A stop by a signal sends no
# report — the unit then ends failed, which shows it — and is recorded as
# failed ("stopped: by SIG…") only once the run holds the maintenance lock
# and is not a dry run: stopped while it waits for the lock, it had not
# begun (bd DAS-Backup-Manager-oeo).
#
# cleanup(), the EXIT trap, exits with exactly one of these: errexit is off
# inside it, so nothing failing there (a log line it cannot write) can end it
# early with a status of its own. A dry run follows the same rule.
# `btrdasd backup run` (CLI, GUI) is not run by the units and keeps its own
# codes: 0, or 1 for any failure.

set -euo pipefail

# ============================================================================
# SINGLE-INSTANCE GUARD
# ============================================================================
# Refuse to run a second backup if another das-backup invocation is already in
# progress. /run is tmpfs (auto-cleared at boot) so the lockfile can't go stale
# across reboots. Exit 0 (not failure) when locked so that cachyos-sentinel
# does not interpret a skipped concurrent fire as a unit failure needing retry.
#
# Only a lock another run HOLDS is a skip. A lock that cannot be opened or
# taken at all — /run missing, no permission, ENOLCK — is "could not start",
# exit 1, and says why: skipping with 0 then would disable every backup
# without a word for as long as the lock stayed broken
# (bd DAS-Backup-Manager-ismb). `-E 75` gives "held" a status no failure
# returns: util-linux flock exits 64, 65 or 71 for its own errors, but a flock
# that answers 1 to everything must not read as "held".
LOCKFILE="/run/das-backup.lock"
if ! exec 9>"$LOCKFILE"; then
    echo "[ERROR] Cannot open the backup lock $LOCKFILE — could not start" >&2
    exit 1
fi
lock_rc=0
flock -n -E 75 9 || lock_rc=$?
if ((lock_rc == 75)); then
    echo "[INFO] Another das-backup run holds $LOCKFILE — skipping this invocation" >&2
    exit 0
elif ((lock_rc != 0)); then
    echo "[ERROR] Cannot lock $LOCKFILE (flock exit $lock_rc) — could not start" >&2
    exit 1
fi
# FD 9 stays open for the rest of the script; lock auto-releases when the
# process exits (FD 9 closes), no explicit unlock needed.

# Path of the blocking maintenance lock shared with the scrub engine
# (indexer/src/scrub.rs MAINTENANCE_LOCK_PATH). MUST match exactly — this is
# the backup side of the mutual-hold interlock. Acquired on fd 8 by
# acquire_maintenance_lock() (defined below, called early in main(), after
# this singleton but before any mount work).
MAINTENANCE_LOCKFILE="/run/das-maintenance.lock"

# ============================================================================
# CONFIGURATION (loaded from config.toml via btrdasd)
# ============================================================================

# Load configuration from config.toml via btrdasd
BTRDASD_BIN="${BTRDASD_BIN:-/usr/bin/btrdasd}"
DAS_CONFIG="${DAS_CONFIG:-/etc/das-backup/config.toml}"
# Boot archive pruner — sibling script, same install directory as this one.
BOOT_ARCHIVE_CLEANUP_BIN="${BOOT_ARCHIVE_CLEANUP_BIN:-/usr/lib/das-backup/boot-archive-cleanup.sh}"

# Load the target/source configuration from config.toml via btrdasd. Called
# once at startup and again after sync_subvolumes, because sync may add a
# source (the per-volume adoption source) and every array below is built from
# this output. Each array is redeclared empty so a reload never keeps an entry
# the config no longer has.
#
# Scalars read at startup straight from the same output (LOG_FILE, GROWTH_LOG,
# LAST_REPORT, ...) are NOT re-derived: sync never changes them, and moving the
# log file mid-run would split one run's log in two.
load_config_env() {
    local env_text
    if ! env_text="$("$BTRDASD_BIN" config dump-env --config "$DAS_CONFIG")"; then
        echo "ERROR: btrdasd could not read $DAS_CONFIG" >&2
        return 1
    fi
    eval "$env_text"

    # Build associative arrays from config
    declare -gA DAS_SERIALS=()           # legacy: anchor serial per label
    declare -gA DAS_SERIALS_LIST=()      # space-separated list of all expected serials
    declare -gA TARGET_MOUNT_UUIDS=()    # BTRFS FS UUID for mount-by-UUID, "" if unset
    declare -gA TARGET_MOUNTS=()
    declare -gA TARGET_NAMES=()
    declare -gA TARGET_ROLES=()
    declare -gA MOUNT_ROLES=()
    local i label_var serial_var serials_var uuid_var mount_var name_var role_var label name_val
    for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
        label_var="DAS_TARGET_${i}_LABEL"
        serial_var="DAS_TARGET_${i}_SERIAL"
        serials_var="DAS_TARGET_${i}_SERIALS"
        uuid_var="DAS_TARGET_${i}_MOUNT_UUID"
        mount_var="DAS_TARGET_${i}_MOUNT"
        name_var="DAS_TARGET_${i}_DISPLAY_NAME"
        role_var="DAS_TARGET_${i}_ROLE"
        label="${!label_var}"
        DAS_SERIALS[$label]="${!serial_var}"
        # SERIALS list (new env var) — fall back to legacy single SERIAL when not emitted
        DAS_SERIALS_LIST[$label]="${!serials_var:-${!serial_var}}"
        TARGET_MOUNT_UUIDS[$label]="${!uuid_var:-}"
        TARGET_MOUNTS[$label]="${!mount_var}"
        TARGET_ROLES[$label]="${!role_var}"
        MOUNT_ROLES[${!mount_var}]="${!role_var}"
        name_val="${!name_var:-}"
        if [[ -n "${name_val:-}" ]]; then
            TARGET_NAMES[${!mount_var}]="$name_val"
        else
            TARGET_NAMES[${!mount_var}]="$label"
        fi
    done

    # Source volumes and devices from config
    declare -gA SOURCE_VOLUMES=()
    declare -gA SOURCE_DEVICES=()
    declare -gA SOURCE_SNAPSHOT_DIRS=()
    local vol_var dev_var snap_var snap_val
    for (( i=0; i<DAS_SOURCE_COUNT; i++ )); do
        label_var="DAS_SOURCE_${i}_LABEL"
        vol_var="DAS_SOURCE_${i}_VOLUME"
        dev_var="DAS_SOURCE_${i}_DEVICE"
        snap_var="DAS_SOURCE_${i}_SNAPSHOT_DIR"
        SOURCE_VOLUMES[${!label_var}]="${!vol_var}"
        SOURCE_DEVICES[${!label_var}]="${!dev_var}"
        snap_val="${!snap_var}"
        if [[ -n "${snap_val:-}" ]]; then
            SOURCE_SNAPSHOT_DIRS[${!label_var}]="$snap_val"
        fi
    done

    # All target mount points (space-separated string from config -> array)
    IFS=' ' read -ra ALL_TARGET_MOUNTS <<< "$DAS_ALL_TARGET_MOUNTS"
}

if [[ -x "$BTRDASD_BIN" ]]; then
    load_config_env || exit 1
else
    echo "ERROR: btrdasd not found at $BTRDASD_BIN" >&2
    exit 1
fi

# Logging (now from config)
LOG_FILE="$DAS_LOG_FILE"

# Email and growth tracking (now from config)
# Reports are submitted unauthenticated to the local mail relay named by
# DAS_EMAIL_SMTP_HOST/PORT (exported by `btrdasd config dump-env`). This script
# holds no mail credential — the relay authenticates upstream by envelope sender
# using a key readable only by root. See .claude/rules/backup.md §Email Reports.
GROWTH_LOG="$DAS_GROWTH_LOG"
# Machine-readable throughput history, one JSON object per run, beside the
# growth log. bd DAS-Backup-Manager-6lr.
DAS_THROUGHPUT_LOG="${DAS_THROUGHPUT_LOG:-$(dirname "$DAS_GROWTH_LOG")/throughput.jsonl}"
LAST_REPORT="$DAS_LAST_REPORT"

# Throughput tracking (populated at runtime)
declare -A USAGE_BEFORE=()
declare -A USAGE_AFTER=()
BTRBK_START_TIME=0
BTRBK_END_TIME=0

# Operation status tracking (for email report)
declare -A OP_STATUS=()

# Report-time cache — populated by capture_report_data() while targets are
# still mounted, BEFORE unmount_all() runs in main(). generate_capacity_
# section()/generate_growth_section() and the LATEST SNAPSHOTS line in
# generate_report() read ONLY these cached values (never a live mountpoint/
# df/btrbk call), so the report can be built after unmount without losing
# data and an unmount failure still lands in the same run's email. Tracks
# bd DAS-Backup-Manager-ecg.
declare -A CAPACITY_USED=()
declare -A CAPACITY_AVAIL=()
declare -A CAPACITY_PCT=()
declare -A AVAIL_BYTES=()
REPORT_TARGETS=()
BTRBK_LATEST=""
BTRBK_LATEST_RAW=""

# Track whether backup run has been recorded (prevents double-recording)
BACKUP_RUN_RECORDED="false"
# Track whether we're in a real (non-dryrun) backup
BACKUP_MODE_REAL="false"
# Track force_full for cleanup trap access
BACKUP_FORCE_FULL="false"
# Set as the LAST statement of main() on the clean-completion path (both
# real-run and dryrun). While "false", the EXIT trap (cleanup()) treats
# termination as an abort and runs its full recovery body; once "true",
# cleanup() no-ops immediately (unmount_all/record_backup_run_in_db already
# ran in main()'s own body for a clean run). bd DAS-Backup-Manager-oeo.
SCRIPT_COMPLETED="false"
# Targets configured but not present this run. Recorded so an absent
# target produces a row in the report instead of merely being missing
# from every section — an absent row is what nobody notices. bd nsp (c6).
UNAVAILABLE_TARGETS=()
# "true" once capture_report_data() has read the snapshot counters, "false"
# when that read failed, so an empty RAW is not read as "nothing was sent"
# (bd nsp c4). Empty until then: a run that ends before the read records its
# counts as unknown, never as 0 (bd DAS-Backup-Manager-6wt).
BTRBK_LATEST_RAW_OK=""
# The snapshot counts this run reports, as record-run arguments — set by
# decide_run_counts (bd DAS-Backup-Manager-bzw).
RUN_COUNTS=()
# Set inside main() the moment this process actually OWNS the shared DAS
# mountpoints — immediately after acquire_maintenance_lock() returns (see
# the arming site in main() for the exact line and full rationale). Until
# then, cleanup() must NOT run its recovery body even though the EXIT trap
# is already installed and ALL_TARGET_MOUNTS is already populated (both
# happen well before main() is entered): indexer/src/scrub.rs mounts the
# SAME /mnt/backup-* paths under the protection of that same maintenance
# lock, and states its own invariant explicitly — "a backup can never
# unmount a filesystem out from under a running scrub". An abort inside
# main() BEFORE the maintenance lock is held (a `--typo` usage error,
# check_root() failing) would otherwise call unmount_all() over those same
# paths with no exclusivity guarantee against a live scrub pass holding one
# of them. CLEANUP_ARMED is what keeps that invariant true on backup's
# abort paths, not just its happy path. bd DAS-Backup-Manager-oeo.
CLEANUP_ARMED="false"
# What stopped a run that aborted with 3 before its report — the step, and
# why, one line per finding — for cleanup()'s ABORTED report and history row
# (bd DAS-Backup-Manager-2my). Set by abort_reason, or by note_abort for a
# command that failed under set -e.
ABORT_WHAT=""
ABORT_REASON=""
# "true" once main() has sent its report: an abort after that has a report
# already, and sends no second one.
REPORT_SENT="false"
# "true" when send_report() wrote the report it was last given to
# $LAST_REPORT; "false" when that write failed (report_whereabouts).
REPORT_SAVED="false"
# Which targets check_das_connected found ("true") or not ("false"), keyed by
# label. Declared here, empty, so the abort report can read it under set -u
# even when the run stopped before detection.
declare -A TARGET_AVAILABLE=()
# The source mount points THIS run mounted and has not unmounted yet, in the
# order it mounted them: one entry per mount point, however many sources
# share it (nvme and nvme-vm share /.btrfs-nvme). unmount_all() unmounts
# these and nothing else. A source volume already mounted when
# mount_sources() looked — an fstab mount such as /dasRaid0 or the /.btrfs-*
# top levels, or one made by hand — is used as found and is never this run's
# to unmount (bd DAS-Backup-Manager-8cf). Kept apart from SOURCE_VOLUMES,
# which the reload after subvolume sync declares anew.
SOURCE_MOUNTS_OWNED=()

# Colors for interactive output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# ============================================================================
# FUNCTIONS
# ============================================================================

log() {
    local level="$1"
    local msg="$2"
    local timestamp
    timestamp=$(date '+%Y-%m-%d %H:%M:%S')
    echo "[$timestamp] [$level] $msg" >> "$LOG_FILE"

    case "$level" in
        INFO)  echo -e "${GREEN}[INFO]${NC} $msg" ;;
        WARN)  echo -e "${YELLOW}[WARN]${NC} $msg" ;;
        ERROR) echo -e "${RED}[ERROR]${NC} $msg" ;;
    esac
}

log_info()  { log "INFO" "$1"; }
log_warn()  { log "WARN" "$1"; }
log_error() { log "ERROR" "$1"; }

# Track operation status for the email report
record_op() {
    local op="$1" result="$2" detail="${3:-}"
    OP_STATUS[$op]="$result"
    if [[ -n "$detail" ]]; then
        OP_STATUS["${op}_detail"]="$detail"
    fi
}

check_root() {
    if [[ $EUID -ne 0 ]]; then
        log_error "This script must be run as root"
        exit 1
    fi
}

# Acquire the blocking maintenance lock — the backup side of the mutual-hold
# interlock shared with the scrub engine (indexer/src/scrub.rs). Must be
# called AFTER the fd-9 singleton lock succeeds and BEFORE any mount work,
# held open (fd 8) to process exit — same "open once, let process exit
# release it" lifetime as the singleton lock above.
#
# A short non-blocking probe runs first so ordinary sub-second contention
# stays quiet. If the lock is still held, a visible line is logged and the
# call falls through to a genuinely blocking flock. This is a deferral,
# never a cancellation: a backup whose scheduled time lands mid-scrub waits
# and starts the moment the scrub releases the lock. The wait duration is
# logged and recorded via record_op so a non-trivial wait is visible in the
# journal (and, for a same-run report, the email). Mirrors scrub.rs's
# acquire_blocking() announce-then-block pattern (5s announce threshold);
# the 15-minute re-announce loop it also has is not reproduced here for
# simplicity — a single "waiting" line plus the acquired-with-duration line
# meets the brief's documented "at least" bar and keeps this function free
# of background-job cleanup edge cases.
#
# Opened `<>`, not `>`: the file holds the current holder's record (see
# record_maintenance_holder), and `>` would empty it on open — before this
# run holds anything, while it merely waits. flock(1) locks the open file
# description whatever its mode, so the locking is unchanged (bd frb).
acquire_maintenance_lock() {
    # fd 8 is not close-on-exec, so every child this run starts inherits it
    # and shares the hold: a child that outlives this run keeps the lock
    # held until it exits too. Only run_indexer relies on that, and says so
    # (DAS_MAINTENANCE_LOCK_FD=8 for `btrdasd walk`); for every other child
    # the inheritance is incidental and harmless while children end before
    # the run does. Never `flock -u 8` while a child may hold the
    # description — that releases it for the child as well.
    exec 8<>"$MAINTENANCE_LOCKFILE"
    if flock -n 8; then
        record_maintenance_holder
        return
    fi

    # Mirrors scrub.rs's LOCK_WAIT_ANNOUNCE_SECS=5s probe before announcing.
    sleep 5
    if flock -n 8; then
        record_maintenance_holder
        return
    fi

    log_info "DAS maintenance lock held by $(maintenance_holder) — waiting..."
    local wait_start
    wait_start=$(date +%s)

    flock 8
    record_maintenance_holder

    local wait_secs
    wait_secs=$(( $(date +%s) - wait_start ))
    log_info "DAS maintenance lock acquired after waiting ${wait_secs}s"
    record_op "lock_wait" "OK" "waited ${wait_secs}s for $MAINTENANCE_LOCKFILE"
}

# Record this run as the holder of the maintenance lock, in the lock file
# itself, so a restore or index job that finds the lock held can say what it
# waits for — as every holder in indexer/src/maintenance.rs does. Called only
# while holding the lock. Display only: a failed write is logged, and the run
# goes on. cleanup() empties it again (clear_maintenance_holder).
record_maintenance_holder() {
    if ! printf 'backup-run.sh pid %s\n' "$$" >"$MAINTENANCE_LOCKFILE"; then
        log_warn "Could not record this run as the holder of $MAINTENANCE_LOCKFILE"
    fi
}

# Empty this run's record, while fd 8 still holds the lock, so a finished run
# is never named as the holder. Called by cleanup() only once the lock is
# this run's (CLEANUP_ARMED); before that the record is another holder's.
# Emptied, never removed: the file is the lock. Display only: a failure is
# logged, and a reader then sees that the recorded pid has gone.
# Called only by cleanup(), the EXIT trap. shellcheck 0.11 stops seeing a
# trap's handlers as called once the last line ends in `exit` (SC2329).
# shellcheck disable=SC2329
clear_maintenance_holder() {
    if ! : >"$MAINTENANCE_LOCKFILE"; then
        log_warn "Could not empty the holder record in $MAINTENANCE_LOCKFILE"
    fi
}

# Who holds the maintenance lock, as its holder recorded itself — the reading
# `holder_of` in indexer/src/maintenance.rs gives, with the same four answers
# (tests/test_maintenance_lock.sh pins them as that file's tests do): nothing
# recorded; a record without a pid, which is only the last one recorded; a
# record whose process has gone; and a live holder, named as recorded. First
# line only, without control characters, at most 200 characters. Display
# only: a file that cannot be read reads as nothing recorded.
maintenance_holder() {
    local line="" pid
    if [[ -r "$MAINTENANCE_LOCKFILE" ]]; then
        IFS= read -r line <"$MAINTENANCE_LOCKFILE" || true
    fi
    line="${line//[[:cntrl:]]/}"
    line="${line:0:200}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    if [[ -z "$line" ]]; then
        echo "an unknown holder"
        return
    fi
    pid="${line##* pid }"
    if [[ "$line" != *" pid "* || ! "$pid" =~ ^[[:digit:]]+$ ]]; then
        echo "an unknown holder (last recorded: $line)"
    elif [[ -d "/proc/$pid" ]]; then
        echo "$line"
    else
        echo "an unknown holder (the last recorded holder, $line, is no longer running)"
    fi
}

# Find device by serial number
find_device_by_serial() {
    local serial="$1"
    local dev dev_serial
    for dev in /dev/sd[a-z] /dev/sd[a-z][a-z]; do
        if [[ -b "$dev" ]]; then
            dev_serial=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3}')
            if [[ "$dev_serial" == "$serial" ]]; then
                echo "$dev"
                return 0
            fi
        fi
    done
    return 1
}

check_das_connected() {
    log_info "Detecting DAS drives by serial number..."

    # Each target may have one or more expected serials (RAID-1 → 2). For each
    # target we record the *first* device found in DISCOVERED_DEVICES (used by
    # legacy code paths) and the *availability* in TARGET_AVAILABLE.
    #
    # A target is AVAILABLE when:
    #   - mount_uuid is set, AND any expected serial is present OR the FS UUID
    #     itself is discoverable on the system (BTRFS multi-device superblock
    #     can satisfy this from the surviving leg alone)
    #   - or, mount_uuid is unset (legacy targets) AND at least one expected
    #     serial is present
    #
    # An UNAVAILABLE primary target aborts the run; an unavailable mirror is
    # skipped with a warning. RAID-1 partial-membership is degraded, not an
    # outage — we proceed with a warning.
    declare -gA DISCOVERED_DEVICES=()
    declare -gA TARGET_AVAILABLE=()
    local any_primary_available="false"
    local any_primary_configured="false"

    for label in "${!DAS_SERIALS[@]}"; do
        local serials_list="${DAS_SERIALS_LIST[$label]}"
        local uuid="${TARGET_MOUNT_UUIDS[$label]}"
        local role="${TARGET_ROLES[$label]}"
        if [[ "$role" == "primary" ]]; then
            any_primary_configured="true"
        fi

        local first_dev=""
        local -a found_devs=()
        local -a missing_serials=()
        local -a found_serials=()
        local serial
        for serial in $serials_list; do
            local dev
            dev=$(find_device_by_serial "$serial") || dev=""
            if [[ -n "$dev" ]]; then
                found_devs+=("$dev")
                found_serials+=("$serial")
                if [[ -z "$first_dev" ]]; then
                    first_dev="$dev"
                fi
            else
                missing_serials+=("$serial")
            fi
        done

        local available="false"
        # Path 1: mount_uuid known → BTRFS can satisfy from any one leg
        if [[ -n "$uuid" ]]; then
            if (( ${#found_devs[@]} > 0 )); then
                available="true"
            elif blkid -U "$uuid" >/dev/null 2>&1; then
                # FS UUID resolves on this host even without a configured serial
                # match (e.g. a replacement drive with a yet-unknown serial).
                available="true"
                first_dev="$(blkid -U "$uuid" 2>/dev/null | sed 's/[0-9]*$//')"
            fi
        else
            # Path 2: legacy device-by-serial only
            if (( ${#found_devs[@]} > 0 )); then
                available="true"
            fi
        fi

        TARGET_AVAILABLE[$label]="$available"
        if [[ "$available" == "true" ]]; then
            DISCOVERED_DEVICES[$label]="$first_dev"
            if (( ${#missing_serials[@]} > 0 )); then
                log_warn "  $label: present=[${found_serials[*]}] missing=[${missing_serials[*]}] — RAID-1 degraded, proceeding"
            else
                log_info "  $label: $first_dev (${found_serials[*]:-<uuid-only>}) — $role"
            fi
            if [[ "$role" == "primary" ]]; then
                any_primary_available="true"
            fi
        else
            if [[ "$role" == "primary" ]]; then
                log_error "  $label ($role): no expected drives present and no mountable UUID"
                log_error "    expected serials: ${serials_list:-<none>}"
                log_error "    mount UUID:       ${uuid:-<unset>}"
            else
                # An unavailable non-primary used to be logged once here and
                # then be absent from EVERY report section -- throughput,
                # capacity, growth and SMART all iterate over mounted targets
                # only. The absence of a row was the entire signal, and an
                # absent row is what nobody notices: a recovery drive could
                # fall off the bus and every nightly email would still read
                # ALL OPERATIONS SUCCESSFUL. bd nsp (c6).
                log_warn "  $label ($role): no expected drives present — will skip"
                record_op "target_${label}" "FAIL" "unavailable: no expected drive present and no mountable UUID"
                UNAVAILABLE_TARGETS+=("$label")
            fi
        fi
    done

    # Only abort when at least one primary was configured AND none are reachable.
    # 3, not 1: the run holds the lock and has begun; the next start would
    # find the DAS just as absent (EXIT STATUS, bd DAS-Backup-Manager-d1r).
    if [[ "$any_primary_configured" == "true" && "$any_primary_available" != "true" ]]; then
        log_error "No primary backup target is available — aborting"
        log_error "Is the DAS connected and powered on?"
        abort_reason "DAS detection" "no primary backup target is available — is the DAS connected and powered on?"
        exit 3
    fi
}

set_io_scheduler() {
    log_info "Setting I/O scheduler to $DAS_IO_SCHEDULER for DAS drives..."

    for label in "${!DISCOVERED_DEVICES[@]}"; do
        local drive="${DISCOVERED_DEVICES[$label]}"
        if [[ -n "$drive" && -b "$drive" ]]; then
            local dev="${drive#/dev/}"
            if [[ -f "/sys/block/$dev/queue/scheduler" ]]; then
                echo "$DAS_IO_SCHEDULER" > "/sys/block/$dev/queue/scheduler" 2>/dev/null || true
            fi
        fi
    done
}

create_mount_points() {
    log_info "Creating mount points..."
    for label in "${!SOURCE_VOLUMES[@]}"; do
        mkdir -p "${SOURCE_VOLUMES[$label]}"
    done

    # Only create mountpoint dirs for targets we actually plan to mount.
    # Bare directories at $mnt are a foot-gun: btrbk's config references
    # them, and if the underlying DAS filesystem isn't mounted there at
    # backup time, btrbk will silently write to the bare directory on /
    # (the NVMe root), filling the root filesystem. This happened in May
    # 2026 when the original 22TB drive was removed and a backup ran
    # before the replacement was in place — see bd DAS-Backup-Manager-9on.
    #
    # For UNAVAILABLE targets, proactively rmdir any pre-existing empty
    # mountpoint dir so btrbk's path is non-existent at write time
    # (btrbk fails fast and safely). If the dir is non-empty, that's
    # evidence a prior write hit the bare dir — refuse to proceed.
    # Both refusals exit 3: a target's state, met again by the next start
    # (EXIT STATUS, bd DAS-Backup-Manager-d1r).
    for label in "${!TARGET_MOUNTS[@]}"; do
        local mnt="${TARGET_MOUNTS[$label]}"
        local available="${TARGET_AVAILABLE[$label]:-false}"
        if [[ "$available" == "true" ]]; then
            mkdir -p "$mnt"
            continue
        fi

        if [[ ! -e "$mnt" ]]; then
            continue   # already non-existent — safe, nothing to do
        fi

        # Already mounted from a prior run? Try to unmount cleanly first.
        # (Should not happen given mount_targets symmetry, but be defensive.)
        if mountpoint -q "$mnt" 2>/dev/null; then
            log_warn "  $label is unavailable but $mnt is currently mounted — attempting umount"
            if ! umount "$mnt" 2>/dev/null; then
                log_error "  $label: refusing to proceed — $mnt is mounted but target is marked unavailable"
                abort_reason "mount point preparation" "$label is unavailable, but $mnt is mounted and will not unmount"
                exit 3
            fi
        fi

        # Now $mnt should be a bare directory. Remove it if empty.
        if rmdir "$mnt" 2>/dev/null; then
            log_info "  Removed pre-existing bare mountpoint $mnt (target unavailable)"
        else
            log_error "  ABORTING: target $label is unavailable but $mnt is non-empty."
            log_error "  This is the failure pattern that filled / in May 2026: a prior backup"
            log_error "  wrote data to a bare directory because the DAS target wasn't mounted."
            log_error "  Inspect $mnt manually, move/delete its contents (likely on /), and re-run."
            log_error "  See bd DAS-Backup-Manager-9on."
            abort_reason "the bare-mountpoint guard" \
                "$label is unavailable, but $mnt is not empty: an earlier run may have written there, on the root filesystem"
            exit 3
        fi
    done
}

# Whether this run mounted <mount point> and still holds it.
owns_source_mount() { # owns_source_mount <mount point>
    local m
    for m in "${SOURCE_MOUNTS_OWNED[@]}"; do
        if [[ "$m" == "$1" ]]; then
            return 0
        fi
    done
    return 1
}

# Strike <mount point> from what this run holds: it is unmounted, or the
# mount that was to make it the run's failed.
disown_source_mount() { # disown_source_mount <mount point>
    local m
    local -a kept=()
    for m in "${SOURCE_MOUNTS_OWNED[@]}"; do
        if [[ "$m" != "$1" ]]; then
            kept+=("$m")
        fi
    done
    SOURCE_MOUNTS_OWNED=("${kept[@]}")
}

# Mount each source volume that is not mounted, at its top level, and record
# each mount point this run mounts in SOURCE_MOUNTS_OWNED — the only source
# mounts unmount_all() ever takes down (bd DAS-Backup-Manager-8cf).
mount_sources() {
    log_info "Mounting source top-level volumes..."

    local label mnt dev mount_rc mount_err
    local -a opts
    for label in "${!SOURCE_VOLUMES[@]}"; do
        mnt="${SOURCE_VOLUMES[$label]}"
        dev="${SOURCE_DEVICES[$label]}"
        if mountpoint -q "$mnt"; then
            # Mounted already: by this run, for another source on the same
            # mount point, or before the run, by fstab or by hand. One found
            # mounted is used as found and never unmounted by this run;
            # verify_sources_before_write() still requires it to be the
            # expected filesystem at its top level, or the run aborts.
            if ! owns_source_mount "$mnt"; then
                log_info "  $label: $mnt was already mounted — used as found; this run will not unmount it"
            fi
            continue
        fi
        # fstab may declare this mount point (it declares the /.btrfs-* top
        # levels). Then fstab's own mount is missing, and only this run's
        # helper stands in for it, until the run takes it down. Said, not
        # warned: restoring it is the operator's, not the backup's. Display
        # only: a query that fails says nothing.
        if findmnt --fstab -n -o TARGET --mountpoint "$mnt" >/dev/null 2>&1; then
            log_info "  $label: fstab mounts $mnt at boot, but it was not mounted — this run mounts a helper there and takes it down at the end"
        fi
        opts=(-o subvolid=5)
        if [[ "$dev" == UUID=* ]]; then
            # UUID-based mount (stable across device letter changes)
            opts=(-t btrfs -o subvolid=5)
        fi
        # The run's from here, recorded BEFORE mount runs: a stop that lands
        # while it runs (`systemctl stop` signals every process of the unit)
        # still finds it, and unmount_all() takes down a recorded mount point
        # only while it is one. A mount that fails is struck off again before
        # the abort, so cleanup() leaves alone whatever is mounted there.
        SOURCE_MOUNTS_OWNED+=("$mnt")
        # A source that will not mount ends the run (3: a source's state
        # the next start meets again), named: which source, its device
        # and mount's own message. Left to set -e, the report and the
        # history said only `mount -t btrfs -o subvolid=5 "$dev" "$mnt"`,
        # unexpanded (bd DAS-Backup-Manager-d1r, round 3: M4).
        mount_rc=0
        mount_err="$(mount "${opts[@]}" "$dev" "$mnt" 2>&1)" || mount_rc=$?
        if ((mount_rc != 0)); then
            disown_source_mount "$mnt"
            log_error "Cannot mount source $label ($dev) at $mnt — mount exited $mount_rc: ${mount_err:-no message}"
            abort_reason "source mount" "$label: $dev at $mnt: mount exited $mount_rc: ${mount_err:-no message}"
            exit 3
        fi
        # A mount that succeeds can still have something to say —
        # util-linux's "source write-protected, mounted read-only" — and
        # it used to reach the journal on its own. Captured above, it is
        # passed on (bd DAS-Backup-Manager-d1r, round 4: N1).
        if [[ -n "$mount_err" ]]; then
            log_warn "  mount said, mounting $label ($dev) at $mnt: ${mount_err//$'\n'/; }"
        fi
        log_info "  Mounted $label at $mnt"
    done
}

# Verify EVERY configured source volume is a real mountpoint backed by the
# filesystem config.toml declares for it, BEFORE anything writes to a source
# path.
#
# The target-side twin of this guard (verify_targets_before_btrbk, below) has
# existed since the May 2026 incident. Sources had nothing equivalent:
# mount_sources() runs `mountpoint -q`, mounts if absent, logs success, and
# nothing ever checked WHAT got mounted.
#
# That gap is not theoretical. An empty /.btrfs-hdd/.btrbk-snapshots directory
# dated 2026-05-17 was found on the NVMe ROOT filesystem — proof that
# create_snapshot_dirs() had run at least once while /.btrfs-hdd was unmounted,
# writing to the bare mountpoint. It stayed bounded to an empty directory only
# because btrbk then finds no source subvolumes and exits nonzero rather than
# filling /. This script unmounts at the end of every run the source volumes
# it mounted itself (bd DAS-Backup-Manager-8cf), so between runs a source path
# that nothing else mounts IS bare, and anything writing to it lands on the
# root filesystem. And a source found mounted is used as found, so whatever
# it holds must be checked as well. Tracks bd DAS-Backup-Manager-zlv
# (source-side sibling of bd DAS-Backup-Manager-9on).
#
# Two checks, and the ORDER matters:
#
#   1. `mountpoint -q` — the load-bearing one. A bare source mountpoint falls
#      through to whatever filesystem contains it (the NVMe root, here), and
#      `findmnt --target` on a bare path reports THAT filesystem's UUID quite
#      happily. For the `nvme` source the root filesystem IS the source
#      filesystem — /dev/nvme1n1p2 and / are both UUID=20b5fa7e-…, the source
#      is just its subvolid=5 view — so check 2 below CANNOT tell "mounted"
#      from "bare" there. Measured on this host with /.btrfs-nvme unmounted:
#      `findmnt -n -o UUID --target /.btrfs-nvme` → 20b5fa7e-…, i.e. a UUID
#      match on a bare directory. Only `mountpoint -q` distinguishes them. Do
#      not drop or reorder this on the grounds that the UUID comparison
#      subsumes it — for that source it does not.
#
#   2. Filesystem identity — the mounted UUID must match what the source's
#      `device` resolves to, and the mount must be the top-level volume
#      (FSROOT `/`), which is what `mount -o subvolid=5` in mount_sources()
#      produces and what btrbk.conf's per-source subvolume paths are relative
#      to. A mount rooted at a subvolume (`/@`, `/@home`, a named subvol)
#      would resolve btrbk's declared paths against the wrong tree.
#
# HONEST SCOPE OF CHECK 2, by `device` form — this paragraph exists so nobody
# later reads the path form as stronger than it is:
#
#   device = "UUID=<uuid>"  A genuine identity check. The expected UUID is
#                           declared in config.toml and compared against the
#                           filesystem actually mounted. Nothing the running
#                           system does can move it. Seven of the nine live
#                           sources are this form.
#
#   device = "/dev/..."     NOT an identity check. There is no stable
#                           identifier in the config to compare against, so
#                           the expected UUID is resolved FROM THE DEVICE NODE
#                           AT VERIFICATION TIME via blkid. That proves the
#                           mount is consistent with whatever disk sits at that
#                           path right now; it cannot detect that the path now
#                           refers to a different disk than the operator meant
#                           (kernel device names are not stable — the whole
#                           reason the target side and most sources use UUID=
#                           form). The live config has two such sources,
#                           `nvme` and `nvme-vm`, both /dev/nvme1n1p2 — which
#                           is one LEG of a two-device BTRFS RAID-1, so blkid
#                           there reports the filesystem UUID shared with
#                           /dev/nvme0n1p2. The path therefore names a member,
#                           not a filesystem, and a member is exactly what
#                           kernel naming is free to reassign. Migrating both
#                           to UUID=20b5fa7e-… would upgrade them to a real
#                           identity check. Until then check 1 is the only
#                           guarantee those two get — and check 1 is precisely
#                           the one that catches the defect this guard was
#                           written for.
#
# Fails CLOSED: an unresolvable device, a missing device, or a findmnt that
# returns nothing is a violation, not a pass.
#
# Called from main() between mount_sources() and create_snapshot_dirs() —
# see the call site for why that placement and not next to the target guard.
verify_sources_before_write() {
    log_info "Verifying every source volume is backed by its expected filesystem..."

    local violations=()
    local label
    for label in "${!SOURCE_VOLUMES[@]}"; do
        local mnt="${SOURCE_VOLUMES[$label]:-}"
        local dev="${SOURCE_DEVICES[$label]:-}"

        if [[ -z "$mnt" ]]; then
            violations+=("$label: no volume path configured — cannot verify anything")
            continue
        fi

        # Check 1 (see header): the ONLY check that catches a bare source
        # mountpoint when the source filesystem and the root filesystem are
        # the same filesystem.
        if ! mountpoint -q "$mnt" 2>/dev/null; then
            violations+=("$label: $mnt is NOT a mountpoint — writes here would land on the root filesystem")
            continue
        fi

        # Expected filesystem UUID, and where it came from (kept for the log
        # line so a reader can see at a glance which sources got the strong
        # config-declared check and which got the weaker blkid resolution).
        local expected_uuid="" uuid_source=""
        if [[ "$dev" == UUID=* ]]; then
            expected_uuid="${dev#UUID=}"
            uuid_source="config"
        elif [[ -n "$dev" ]]; then
            expected_uuid=$(blkid -o value -s UUID "$dev" 2>/dev/null || true)
            uuid_source="blkid $dev"
            if [[ -z "$expected_uuid" ]]; then
                violations+=("$label: device '$dev' has no resolvable filesystem UUID (blkid returned nothing) — cannot verify what is mounted at $mnt")
                continue
            fi
        else
            violations+=("$label: no device configured — cannot verify what is mounted at $mnt")
            continue
        fi

        local mnt_line fs_uuid fs_root
        mnt_line=$(findmnt -n -o UUID,FSROOT --target "$mnt" 2>/dev/null || true)
        if [[ -z "$mnt_line" ]]; then
            violations+=("$label: $mnt is a mountpoint but findmnt could not report its UUID/FSROOT")
            continue
        fi
        read -r fs_uuid fs_root <<< "$mnt_line"

        if [[ "$fs_uuid" != "$expected_uuid" ]]; then
            violations+=("$label: $mnt has fs UUID '${fs_uuid:-<none>}', expected '$expected_uuid' (from $uuid_source) — a different filesystem is mounted here")
            continue
        fi

        if [[ "$fs_root" != "/" ]]; then
            violations+=("$label: $mnt is mounted at subvolume '$fs_root', expected the top-level volume '/' (mount -o subvolid=5) — btrbk's declared subvolume paths would resolve against the wrong tree")
            continue
        fi

        log_info "  $label: OK ($mnt → UUID=$fs_uuid, subvol=$fs_root, expected from $uuid_source)"
    done

    if (( ${#violations[@]} > 0 )); then
        log_error "============================================================"
        log_error "ABORTING — refusing to write to source volumes. Source verification failed:"
        log_error ""
        local v
        for v in "${violations[@]}"; do
            log_error "  - $v"
        done
        log_error ""
        log_error "Continuing would let create_snapshot_dirs() and btrbk write to a bare"
        log_error "mountpoint on the root filesystem, or snapshot from the wrong filesystem."
        log_error "An empty /.btrfs-hdd/.btrbk-snapshots dated 2026-05-17 was found on the"
        log_error "NVMe root for exactly this reason. See bd DAS-Backup-Manager-zlv."
        log_error "============================================================"
        record_op "verify_sources" "FAIL" "${#violations[@]} violation(s)"
        # 3: a source volume's state, met again by the next start (EXIT
        # STATUS, bd DAS-Backup-Manager-d1r).
        abort_reason "source verification" "$(printf '%s\n' "${violations[@]}")"
        exit 3
    fi

    log_info "All source volumes verified — safe to create snapshot dirs and invoke btrbk."
    record_op "verify_sources" "OK"
}

SUBVOL_SYNC_REPORT=""
SUBVOL_EXPIRE_REPORT=""
RECOVERY_OS_REPORT=""
# Where the recovery OS check keeps its last reading for `btrdasd health`;
# the same variable btrdasd itself reads (`btrdasd health`), defaulting to
# the same path as recovery_os's RECOVERY_OS_STATE_PATH.
DAS_RECOVERY_OS_STATE="${DAS_RECOVERY_OS_STATE:-/var/lib/das-backup/recovery-os.json}"
# Upper bound on reading the recovery OSes. A drive stalled in I/O must cost a
# FAIL line, not the run: unmount and the report still have to happen. `-k 10`
# follows the TERM with a KILL (a process stuck in uninterruptible I/O still
# cannot be killed until the I/O returns; nothing in userspace can do that).
RECOVERY_OS_TIMEOUT_SECS=300
# Dry run only: the btrbk.conf the real run would leave, rendered by sync into
# a mktemp file (mode 600) that `btrbk dryrun` reads. Removed by cleanup().
DRYRUN_BTRBK_CONF=""

# Adopt subvolumes that exist and are not excluded; retire entries whose
# subvolume is gone. Runs after verify_sources_before_write, so every source
# volume is mounted and proven to be the expected filesystem.
#
# A failure here never stops the backup: the subvolumes already configured
# must still be backed up. It is recorded, so the report says FAILURES
# DETECTED and backup_runs.success is 0.
sync_subvolumes() {
    local mode="$1"
    local args=(subvol sync --config "$DAS_CONFIG")
    if [[ "$mode" == "dryrun" ]]; then
        args+=(--dry-run)
        # Nothing is written in a dry run, so the btrbk.conf on disk still
        # names what the real run would retire first. btrbk dryrun reads the
        # file sync plans instead (bd DAS-Backup-Manager-g17).
        if DRYRUN_BTRBK_CONF="$(mktemp --tmpdir das-backup-dryrun-btrbk.XXXXXX)"; then
            args+=(--render-btrbk-conf "$DRYRUN_BTRBK_CONF")
        else
            DRYRUN_BTRBK_CONF=""
            log_warn "Could not create a temporary btrbk.conf — the dry run uses the current one"
        fi
    fi

    log_info "Syncing subvolumes with config..."
    local rc=0
    SUBVOL_SYNC_REPORT="$("$BTRDASD_BIN" "${args[@]}")" || rc=$?
    if [[ -n "${DRYRUN_BTRBK_CONF:-}" && ! -s "$DRYRUN_BTRBK_CONF" ]]; then
        log_warn "Sync rendered no btrbk.conf for the dry run — btrbk dryrun uses the current one"
        rm -f -- "$DRYRUN_BTRBK_CONF"
        DRYRUN_BTRBK_CONF=""
    fi
    if [[ $rc -eq 0 ]]; then
        record_op "subvol_sync" "OK"
    else
        record_op "subvol_sync" "FAIL" "exit code $rc — see SUBVOLUME SYNC in the report"
        log_error "Subvolume sync failed (exit $rc); continuing with the existing config"
    fi
    if [[ -n "$SUBVOL_SYNC_REPORT" ]]; then
        local line
        while IFS= read -r line; do log_info "  $line"; done <<<"$SUBVOL_SYNC_REPORT"
    fi

    # Sync may have added a source. Reload even after a failure: a partial
    # success on one volume still changed the config.
    if load_config_env; then
        # The reload can name a source that mount_sources and the first
        # verify_sources_before_write never saw. create_snapshot_dirs is the
        # next writer, so check again: a source on an unmounted volume must
        # abort here, not leave an empty directory on the root filesystem.
        # Only checks, writes nothing, so a second run is safe. It exits 3 on
        # a violation, deliberately not swallowed.
        verify_sources_before_write
    else
        # Keep the sync failure's own detail when there is one.
        if [[ "${OP_STATUS[subvol_sync]}" == "FAIL" ]]; then
            record_op "subvol_sync" "FAIL" "${OP_STATUS[subvol_sync_detail]}; config could not be reloaded after sync"
        else
            record_op "subvol_sync" "FAIL" "config could not be reloaded after sync"
        fi
        log_error "Config could not be reloaded after subvolume sync"
    fi
    return 0
}

# Delete the backups of retired subvolumes that are past their window.
# Runs after btrbk, while the targets are still mounted.
#
# When this run's sync failed it could not look at every volume, so a retired
# subvolume may exist again and be waiting for sync to revive it. Expiry must
# not outrun that: it runs as a dry run, deletes nothing, and the report says
# so. The sync failure already marks the run failed.
expire_retired_subvolumes() {
    local mode="$1"
    local args=(subvol expire --config "$DAS_CONFIG" --db "$DAS_DB_PATH")
    local held_back="false"
    if [[ "$mode" == "dryrun" ]]; then
        args+=(--dry-run)
    elif [[ "${OP_STATUS[subvol_sync]:-}" == "FAIL" ]]; then
        args+=(--dry-run)
        held_back="true"
        log_warn "Subvolume sync failed this run — retired subvolumes are not expired (dry run only)"
    fi

    local rc=0
    SUBVOL_EXPIRE_REPORT="$("$BTRDASD_BIN" "${args[@]}")" || rc=$?
    if [[ $rc -ne 0 ]]; then
        record_op "subvol_expire" "FAIL" "exit code $rc — see RETIRED SUBVOLUMES in the report"
        log_error "Expiry of retired subvolumes failed (exit $rc)"
    elif [[ "$held_back" == "true" ]]; then
        record_op "subvol_expire" "SKIP" "not performed: subvolume sync failed this run (dry run shown)"
    else
        record_op "subvol_expire" "OK"
    fi
    if [[ "$held_back" == "true" && -n "$SUBVOL_EXPIRE_REPORT" ]]; then
        # After the section's header line, before the dry-run lines.
        local header rest
        header="${SUBVOL_EXPIRE_REPORT%%$'\n'*}"
        rest="${SUBVOL_EXPIRE_REPORT#*$'\n'}"
        [[ "$rest" == "$SUBVOL_EXPIRE_REPORT" ]] && rest=""
        SUBVOL_EXPIRE_REPORT="$header"$'\n'"  EXPIRY NOT PERFORMED: subvolume sync failed this run; nothing was deleted (dry run shown)"
        if [[ -n "$rest" ]]; then
            SUBVOL_EXPIRE_REPORT+=$'\n'"$rest"
        fi
    fi
    if [[ -n "$SUBVOL_EXPIRE_REPORT" ]]; then
        local line
        while IFS= read -r line; do log_info "  $line"; done <<<"$SUBVOL_EXPIRE_REPORT"
    fi
    return 0
}

# Read the independent OS on each mounted role=mirror target and say whether
# it has fallen behind the host, and whether booting it would run btrbk. Runs
# after btrbk, while the targets are still mounted: the backup comes first,
# and nothing here can delay or change it. The command only reads under
# <mount>/@; the one file it writes is the state record on the host, and not
# in a dry run.
#
#   exit 0  every inspected OS is current, no WARNING  -> OK
#   exit 1  at least one needs attention: STALE, or    -> WARN (the run succeeded)
#           a WARNING row (btrbk may run at its boot)
#   other   the check could not be done                 -> FAIL, with the reason
#
# The WARN detail says which, from the section's own rows. Never stops the run.
check_recovery_os() {
    local mode="$1"
    local args=(recovery-os status --config "$DAS_CONFIG")
    if [[ "$mode" != "dryrun" ]]; then
        args+=(--state-file "$DAS_RECOVERY_OS_STATE")
    fi

    log_info "Checking the recovery OSes..."
    local rc=0 errf="" err=""
    local bounded=(timeout -k 10 "$RECOVERY_OS_TIMEOUT_SECS" "$BTRDASD_BIN" "${args[@]}")
    errf="$(mktemp --tmpdir das-recovery-os.XXXXXX)" || errf=""
    if [[ -n "$errf" ]]; then
        RECOVERY_OS_REPORT="$("${bounded[@]}" 2>"$errf")" || rc=$?
        err="$(<"$errf")"
        rm -f -- "$errf"
    else
        # No temp file: stderr goes to the journal instead of the detail.
        RECOVERY_OS_REPORT="$("${bounded[@]}")" || rc=$?
    fi
    local first_err="${err%%$'\n'*}"

    # Which drives were not looked at, whether any was, and what an exit 1
    # was for: read from the section the command printed (awk exits 0 on no
    # match).
    local not_mounted inspected stale warned suffix="" why=""
    not_mounted="$(awk '/^  .*  not mounted$/ { sub(/^  /, ""); sub(/  not mounted$/, ""); printf "%s%s", sep, $0; sep = ", " }' <<<"$RECOVERY_OS_REPORT")"
    inspected="$(awk '/^    Result  / { n++ } END { print n + 0 }' <<<"$RECOVERY_OS_REPORT")"
    stale="$(awk '/^    Result  +STALE/ { n++ } END { print n + 0 }' <<<"$RECOVERY_OS_REPORT")"
    warned="$(awk '/^    WARNING  / { n++ } END { print n + 0 }' <<<"$RECOVERY_OS_REPORT")"
    if [[ -n "$not_mounted" ]]; then
        suffix="; not mounted: $not_mounted"
    fi

    case "$rc" in
        0)
            if (( inspected == 0 )); then
                record_op "recovery_os" "OK" "nothing inspected${suffix}"
            else
                record_op "recovery_os" "OK" "nothing needs attention${suffix}"
            fi
            ;;
        1)
            # Exit 1 is "needs attention": a stale OS, a WARNING row (btrbk
            # may run when that OS boots), or both. A section that shows
            # neither still warns, without claiming which.
            if (( stale > 0 )); then
                why="stale"
                log_warn "A recovery OS is behind the host — see RECOVERY OS in the report"
            fi
            if (( warned > 0 )); then
                why="${why:+$why; }btrbk may run at boot"
                log_warn "btrbk may run when a recovery OS boots — see RECOVERY OS in the report"
            fi
            if [[ -z "$why" ]]; then
                why="needs attention"
                log_warn "A recovery OS needs attention — see RECOVERY OS in the report"
            fi
            record_op "recovery_os" "WARN" "$why — see RECOVERY OS in the report${suffix}"
            ;;
        124)
            record_op "recovery_os" "FAIL" "recovery-os status timed out after ${RECOVERY_OS_TIMEOUT_SECS} s (drive stalled?)"
            log_error "The recovery OS check timed out after ${RECOVERY_OS_TIMEOUT_SECS} s (drive stalled?)"
            RECOVERY_OS_REPORT="RECOVERY OS"$'\n'"  CHECK FAILED: ${OP_STATUS[recovery_os_detail]}"
            ;;
        *)
            record_op "recovery_os" "FAIL" "exit code $rc${first_err:+: $first_err}"
            log_error "The recovery OS check failed (exit $rc)${first_err:+: $first_err}"
            if [[ -z "$RECOVERY_OS_REPORT" ]]; then
                RECOVERY_OS_REPORT="RECOVERY OS"$'\n'"  CHECK FAILED: ${OP_STATUS[recovery_os_detail]}"
            fi
            ;;
    esac
    if [[ -n "$err" && "$err" != "$first_err" ]]; then
        local line
        while IFS= read -r line; do log_warn "  $line"; done <<<"$err"
    fi
    if [[ -n "$RECOVERY_OS_REPORT" ]]; then
        local line
        while IFS= read -r line; do log_info "  $line"; done <<<"$RECOVERY_OS_REPORT"
    fi
    return 0
}

# Whether any recorded operation has result $1.
any_op_is() {
    local want="$1" op
    for op in "${!OP_STATUS[@]}"; do
        if [[ "${OP_STATUS[$op]}" == "$want" ]]; then
            return 0
        fi
    done
    return 1
}

# The status recorded for the run: FAILURE only when an operation FAILed. A
# WARN is a finding about something other than this backup, so the run still
# succeeded.
run_status() {
    if any_op_is FAIL; then
        echo "FAILURE"
    else
        echo "SUCCESS"
    fi
}

# The status for the email subject: the run status, with warnings said.
subject_status() {
    if any_op_is FAIL; then
        echo "FAILURE"
    elif any_op_is WARN; then
        echo "SUCCESS WITH WARNINGS"
    else
        echo "SUCCESS"
    fi
}

mount_targets() {
    log_info "Mounting backup targets..."

    for label in "${!TARGET_MOUNTS[@]}"; do
        local mnt="${TARGET_MOUNTS[$label]}"
        local available="${TARGET_AVAILABLE[$label]:-false}"
        local uuid="${TARGET_MOUNT_UUIDS[$label]}"
        local role="${TARGET_ROLES[$label]}"

        if [[ "$available" != "true" ]]; then
            continue
        fi

        if mountpoint -q "$mnt"; then
            continue
        fi

        # Prefer mount-by-UUID when configured. BTRFS auto-discovers all
        # present members from any single one's superblock, so a degraded
        # RAID-1 (one leg missing) still mounts cleanly. The DAS_MOUNT_OPTS
        # config value is expected to include `degraded` for RAID-1 targets.
        if [[ -n "$uuid" ]]; then
            if mount -t btrfs -o "$DAS_MOUNT_OPTS" "UUID=$uuid" "$mnt"; then
                log_info "  Mounted $label at $mnt (UUID=$uuid)"
                continue
            else
                log_warn "  UUID=$uuid mount failed for $label — falling back to device-based mount"
            fi
        fi

        # Legacy device-by-serial path (used when mount_uuid is unset)
        local dev="${DISCOVERED_DEVICES[$label]:-}"
        if [[ -z "$dev" ]]; then
            log_warn "  No discovered device for $label and no UUID configured — skipping"
            continue
        fi

        local part_dev
        if [[ "$role" == "primary" ]]; then
            part_dev="${dev}1"  # Single partition, whole-disk BTRFS
        else
            part_dev="${dev}2"  # Partition 2 for bootable drives (partition 1 = ESP)
        fi

        if [[ ! -b "$part_dev" ]]; then
            log_warn "  Partition $part_dev not found — skipping $label"
            continue
        fi

        if mount -o "$DAS_MOUNT_OPTS" "$part_dev" "$mnt"; then
            log_info "  Mounted $label at $mnt ($part_dev)"
        else
            log_warn "  Could not mount $label at $mnt — btrbk will skip it"
        fi
    done
}

create_snapshot_dirs() {
    # Each [[source]] in config.toml declares snapshot_dir relative to its
    # volume root. Resolve to an absolute path before mkdir so the directory
    # lands inside the source's volume rather than the script's cwd
    # (systemd-started services run with cwd=/, where mkdir would silently
    # create the dir on the root filesystem — see DAS-Backup-Manager-0p2).
    log_info "Creating btrbk snapshot directories..."
    for label in "${!SOURCE_SNAPSHOT_DIRS[@]}"; do
        local snap_dir="${SOURCE_SNAPSHOT_DIRS[$label]}"
        if [[ -z "$snap_dir" ]]; then
            continue
        fi

        local abs_dir
        if [[ "$snap_dir" = /* ]]; then
            abs_dir="$snap_dir"
        else
            local volume="${SOURCE_VOLUMES[$label]:-}"
            if [[ -z "$volume" ]]; then
                log_warn "  $label: no source volume known, cannot resolve snapshot_dir '$snap_dir' — skipping"
                continue
            fi
            abs_dir="${volume%/}/$snap_dir"
        fi

        if ! mkdir -p "$abs_dir" 2>/dev/null; then
            log_warn "  $label: failed to create snapshot_dir $abs_dir (mkdir error)"
            continue
        fi
        log_info "  $label: $abs_dir"
    done
}

# mkdir -p one target directory, saying so when it did not exist yet — on a
# dry run too, where it is the one thing the run creates.
make_target_dir() {
    local dir="$1"
    [[ -d "$dir" ]] && return 0
    mkdir -p "$dir"
    log_info "  Created target directory $dir"
}

# Runs after verify_targets_before_btrbk, so every directory is made inside a
# target that is a real mount point holding the expected filesystem.
create_target_dirs() {
    log_info "Creating target directory structure..."

    # Collect all target subdirs from sources
    local -a all_subdirs=()
    for (( i=0; i<DAS_SOURCE_COUNT; i++ )); do
        local subdirs_var="DAS_SOURCE_${i}_TARGET_SUBDIRS"
        local subdirs="${!subdirs_var:-}"
        if [[ -n "$subdirs" ]]; then
            IFS=' ' read -ra parts <<< "$subdirs"
            all_subdirs+=("${parts[@]}")
        fi
    done

    # Create subdirs on every mounted target
    for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
        local mount_var="DAS_TARGET_${i}_MOUNT"
        local mnt="${!mount_var}"

        if ! mountpoint -q "$mnt" 2>/dev/null; then
            continue
        fi

        for subdir in "${all_subdirs[@]}"; do
            make_target_dir "$mnt/$subdir"
        done

        # Dry run: the planned btrbk.conf may send a pending adoption to a
        # target directory the current config does not name yet (a new
        # <source>-adopted source). The real run creates it here, after its
        # config reload; create it for the dry run too, or btrbk dryrun fails
        # on a path the real run would have (bd DAS-Backup-Manager-g17). Only
        # `target` lines under this mounted target are used.
        if [[ -n "${DRYRUN_BTRBK_CONF:-}" && -f "$DRYRUN_BTRBK_CONF" ]]; then
            local kw path _rest
            while read -r kw path _rest; do
                [[ "$kw" == "target" && "$path" == "$mnt"/* && "$path" != *"/.."* ]] || continue
                make_target_dir "$path"
            done <"$DRYRUN_BTRBK_CONF"
        fi
    done
}

# Verify EVERY configured target is in one of two safe states before invoking
# btrbk:
#   1. Real mountpoint backed by the expected filesystem (UUID match preferred,
#      device-serial match as fallback). btrbk will write to the DAS as
#      intended.
#   2. Non-existent on disk (no directory at $mnt). btrbk will fail safely
#      with a missing-path error for that target rather than writing to /.
#
# Anything else — a bare directory at $mnt, or a mountpoint backed by an
# UNEXPECTED filesystem (wrong UUID, wrong serial, root filesystem
# shadowing the path) — is the May 2026 foot-gun and we ABORT.
#
# This is the last gate before btrbk gets to make irreversible decisions.
# Tracks bd DAS-Backup-Manager-9on.
verify_targets_before_btrbk() {
    log_info "Verifying every backup target is backed by an expected DAS device..."

    local violations=()

    for label in "${!TARGET_MOUNTS[@]}"; do
        local mnt="${TARGET_MOUNTS[$label]}"
        local available="${TARGET_AVAILABLE[$label]:-false}"
        local uuid="${TARGET_MOUNT_UUIDS[$label]}"
        local serials_list="${DAS_SERIALS_LIST[$label]}"
        local role="${TARGET_ROLES[$label]}"

        # State A: target intentionally not active this run (no DAS drive
        # present, mirror role). Confirmed-safe ONLY if $mnt does not exist;
        # otherwise btrbk could still write to a bare dir.
        if [[ "$available" != "true" ]]; then
            if [[ -e "$mnt" ]]; then
                violations+=("$label: marked unavailable but $mnt still exists (would let btrbk write to bare dir on /)")
            fi
            continue
        fi

        # State B: target should be active — MUST be a real mountpoint.
        if ! mountpoint -q "$mnt" 2>/dev/null; then
            violations+=("$label ($role): expected mounted but $mnt is NOT a mountpoint (mount failed silently in mount_targets)")
            continue
        fi

        # State B verification: the mounted filesystem must be the one we
        # expected, not some other filesystem (or worse, the root FS shadowing
        # an unmounted path). Prefer UUID match (works for BTRFS RAID-1 even
        # under degraded mount), fall back to disk serial.
        if [[ -n "$uuid" ]]; then
            local fs_uuid
            fs_uuid=$(findmnt -n -o UUID --target "$mnt" 2>/dev/null || true)
            if [[ "$fs_uuid" != "$uuid" ]]; then
                violations+=("$label: $mnt has fs UUID '${fs_uuid:-<none>}', expected '$uuid' — different filesystem mounted here")
                continue
            fi
            log_info "  $label: OK ($mnt → UUID=$fs_uuid)"
            continue
        fi

        # Legacy device-by-serial verification (used when mount_uuid is unset)
        local mount_src
        mount_src=$(findmnt -n -o SOURCE --target "$mnt" 2>/dev/null || true)
        if [[ -z "$mount_src" ]]; then
            violations+=("$label: $mnt is a mountpoint but findmnt could not resolve its source device")
            continue
        fi
        local mount_dev="${mount_src%%[0-9]*}"   # /dev/sde1 → /dev/sde
        local got_serial
        got_serial=$(smartctl -i "$mount_dev" 2>/dev/null | awk '/Serial Number:/{print $3}' || true)
        local serial_match="false"
        local expected
        for expected in $serials_list; do
            if [[ "$got_serial" == "$expected" ]]; then
                serial_match="true"
                break
            fi
        done
        if [[ "$serial_match" != "true" ]]; then
            violations+=("$label: $mnt mounted from $mount_src (serial='${got_serial:-?}'), expected one of: $serials_list")
            continue
        fi
        log_info "  $label: OK ($mnt → $mount_src, serial=$got_serial)"
    done

    if (( ${#violations[@]} > 0 )); then
        log_error "============================================================"
        log_error "ABORTING — refusing to invoke btrbk. Target verification failed:"
        log_error ""
        local v
        for v in "${violations[@]}"; do
            log_error "  - $v"
        done
        log_error ""
        log_error "Continuing would let btrbk write to a bare directory on the root"
        log_error "filesystem, filling /. This is the May 2026 incident pattern."
        log_error "See bd DAS-Backup-Manager-9on for the full failure-mode writeup."
        log_error "============================================================"
        record_op "verify_targets" "FAIL" "${#violations[@]} violation(s)"
        # 3: a target's state — one that failed to mount included — met
        # again by the next start (EXIT STATUS, bd DAS-Backup-Manager-d1r).
        abort_reason "target verification" "$(printf '%s\n' "${violations[@]}")"
        exit 3
    fi

    log_info "All backup targets verified — safe to invoke btrbk."
    record_op "verify_targets" "OK"
}

run_btrbk() {
    local mode="${1:-run}"

    log_info "Running btrbk ($mode)..."

    if [[ "$mode" == "dryrun" ]]; then
        # Guarded the same way the real-run branch below always was: under
        # set -euo pipefail an unguarded `btrbk dryrun` failure aborted the
        # whole script with no record_op entry at all (bd DAS-Backup-Manager-vuw
        # — a stale source entry in config.toml made this a real, reproducible
        # failure until the config was fixed). Soft-fail instead, matching
        # every other btrbk-adjacent operation in this script.
        # The btrbk.conf sync planned for this run when there is one; the
        # real run will have written exactly that before btrbk starts.
        local conf="${DRYRUN_BTRBK_CONF:-$DAS_BTRBK_CONF}"
        if btrbk -c "$conf" dryrun; then
            record_op "btrbk" "OK" "dryrun"
            log_info "btrbk dryrun completed"
        else
            record_op "btrbk" "FAIL" "dryrun exit code $?"
            log_error "btrbk dryrun failed"
        fi
    else
        if btrbk -c "$DAS_BTRBK_CONF" run; then
            record_op "btrbk" "OK"
            log_info "btrbk completed"
        else
            record_op "btrbk" "FAIL" "exit code $?"
            log_error "btrbk failed"
        fi
    fi
}

update_boot_subvolumes() {
    local force="${1:-false}"
    local updated=0 skipped=0 failed=0
    local mount_rc mount_why
    # One timestamp per run (not per subvolume/target) — matches the Rust
    # archive_boot() format exactly so boot-archive-cleanup.sh's
    # parse_archive_timestamp() can parse either origin's archives.
    local ts
    ts=$(date +%Y%m%dT%H%M%S)

    log_info "Updating stable boot subvolumes..."

    # Update boot subvolumes on PRIMARY targets only — mirror targets are independent
    # bootable systems and must never have their @ replaced with host snapshots.
    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        # Mounted, not mounted, or could not tell (probe_mount_point). Only the
        # second is a target to leave alone; the third is a failure of this
        # step, counted and said. `mountpoint -q ... || continue` read every
        # failure of the probe — a missing mountpoint program (exit 127), no
        # descriptor free for its redirection — as "not mounted": the target
        # was skipped, counted as neither skipped nor failed, and the step
        # recorded OK, 0 updated, 0 skipped (bd DAS-Backup-Manager-jlsz). Not
        # seen here: a descriptor shortage so complete that bash cannot make
        # the pipe of the capture below returns 0 and nothing, which reads as
        # "mounted" — the limit of every capture, unmount_all's probe included.
        mount_rc=0
        mount_why="$(probe_mount_point "$mnt")" || mount_rc=$?
        case "$mount_rc" in
            0) ;;
            1) continue ;;
            *)
                log_error "  Could not tell whether $mnt is mounted — $mount_why; its boot subvolumes were NOT updated"
                (( failed += 1 ))
                continue
                ;;
        esac

        # Skip mirror targets — they have their own OS installations
        local mount_role="${MOUNT_ROLES[$mnt]:-}"
        if [[ "$mount_role" == "mirror" ]]; then
            log_info "  [$(btrfs filesystem label "$mnt" 2>/dev/null || echo "$mnt")] Skipping mirror target (independent OS)"
            (( skipped += 1 ))
            continue
        fi

        local label
        label=$(btrfs filesystem label "$mnt" 2>/dev/null || echo "$mnt")
        # `|| true` guards against `set -o pipefail` + `set -e` killing the run when
        # grep finds zero matches — the empty-string check below is the intended path.
        #
        # Snapshot names come straight from /etc/btrbk/btrbk.conf's per-subvolume
        # `snapshot_name` (nvme volume: @ -> "root-", @home -> "home"), so the
        # on-disk names are "root-.<TS>" and "home.<TS>" -- NOT "root.<TS>". A
        # bare `nvme/root\.` pattern (root immediately followed by a dot) has
        # NEVER matched "root-.<TS>" (bd DAS-Backup-Manager-ycr, pre-existing at
        # e363919, predates this file's v4.x history) -- it silently found zero
        # snapshots on every run, so this whole function no-op'd on every target
        # whose @ subvolume already existed, with no error, just a
        # "No btrbk snapshots found, skipping" warning that looked like a benign
        # first-run condition instead of a permanent pattern bug. Both patterns
        # are anchored to a full `-<TS>`/`.<TS>` timestamp suffix so a rename in
        # btrbk.conf breaks this loudly (empty match -> the skip warning below)
        # rather than silently matching an unrelated subvolume (e.g.
        # "root-root.<TS>", a different snapshot_name entirely, which the old
        # unanchored pattern happened not to match only by the accident of "root"
        # not being immediately followed by "."). bd DAS-Backup-Manager-01u (the
        # config-vs-script drift detector) is the systemic guard against this
        # class of bug recurring for a *different* rename in the future.
        #
        # Timestamp suffix is btrbk's default `timestamp_format long`
        # (btrbk.conf(5)): YYYYMMDD<T>hhmm, i.e. 8 digits + T + 4 digits
        # (HHMM, no seconds) — verified live against real snapshot names
        # below, NOT hhmmss/6 digits. An optional "_N" collision suffix is
        # appended by btrbk if two snapshots land on the exact same minute.
        # The subvolume listing is captured ONCE, with its status checked,
        # before anything is matched against it. Previously each grep ran off
        # its own `btrfs subvolume list ... 2>/dev/null || true`, so a failed
        # listing produced an empty string that was indistinguishable from
        # "this target genuinely has no snapshots" — and the latter is the
        # benign, skip-quietly branch. bd DAS-Backup-Manager-nsp (c2/c3).
        local subvol_listing subvol_err
        subvol_err="$(mktemp)"
        if ! subvol_listing=$(btrfs subvolume list "$mnt" 2>"$subvol_err"); then
            log_error "  [$label] Could not list subvolumes: $(tr '\n' ' ' <"$subvol_err")"
            log_error "  [$label] Refusing to treat an unreadable target as 'no snapshots'"
            rm -f "$subvol_err"
            (( failed += 1 ))
            continue
        fi
        rm -f "$subvol_err"

        local latest_root latest_home
        latest_root=$(printf '%s\n' "$subvol_listing" | grep -E 'nvme/root-\.[0-9]{8}T[0-9]{4}(_[0-9]+)?$' | awk '{print $NF}' | sort | tail -1) || true
        latest_home=$(printf '%s\n' "$subvol_listing" | grep -E 'nvme/home\.[0-9]{8}T[0-9]{4}(_[0-9]+)?$' | awk '{print $NF}' | sort | tail -1) || true

        if [[ -z "$latest_root" || -z "$latest_home" ]]; then
            # A target that HAS snapshots but matches neither pattern means the
            # patterns above have drifted from btrbk.conf's snapshot_name --
            # exactly the shape of bd 5ig on the Rust side, where a hardcoded
            # "@" => "root" map stopped matching an on-disk "root-" prefix.
            # That is a defect, not a skip, so it must not exit through the
            # quiet branch.
            #
            # Matched by bash itself: no pipe, no file, no fd. Piped into
            # grep -q, a listing longer than a pipe holds (the primary
            # target's is near 64 KiB) left printf to die of SIGPIPE, which
            # pipefail read as "no match" (bd DAS-Backup-Manager-wkvz); fed
            # as a here-string, it went to a temp file, and a full /tmp or no
            # fd to spare read as "no match" too (round 4, N3). Either way,
            # straight into the quiet branch. [[:digit:]], not [0-9]: under
            # en_US.UTF-8 bash's regex [0-9] also matches Arabic-Indic and
            # fullwidth digits, where grep's matched ASCII only; [[:digit:]]
            # is ASCII in every locale, the old meaning (round 5, N5).
            if [[ $subvol_listing =~ [[:digit:]]{8}T[[:digit:]]{4} ]]; then
                log_error "  [$label] Target HAS btrbk-shaped snapshots but none matched the expected names."
                log_error "  [$label] The name patterns in this function have drifted from /etc/btrbk/btrbk.conf."
                (( failed += 1 ))
            else
                log_warn "  [$label] No btrbk snapshots found, skipping"
                (( skipped += 1 ))
            fi
            continue
        fi

        log_info "  [$label] Latest root: $latest_root"
        log_info "  [$label] Latest home: $latest_home"

        # Update @ subvolume (archive-then-swap: archive the outgoing @ as a
        # read-only snapshot BEFORE the create-then-swap replacement, so the
        # only copy of the outgoing subvolume is never destroyed. If the
        # archive snapshot fails, skip the recreation entirely for this
        # subvolume — never delete @ without a preserved archive.)
        if btrfs subvolume show "$mnt/@" &>/dev/null; then
            if [[ "$force" == "true" ]]; then
                if ! btrfs subvolume snapshot -r "$mnt/@" "$mnt/@.archive.$ts"; then
                    log_error "  [$label] Failed to archive @ -> @.archive.$ts — skipping recreation (old @ preserved)"
                    (( failed += 1 ))
                elif btrfs subvolume snapshot "$mnt/$latest_root" "$mnt/@.new" && \
                     btrfs subvolume delete "$mnt/@" && \
                     mv "$mnt/@.new" "$mnt/@"; then
                    log_info "  [$label] Recreated @ from $latest_root (archived old @ -> @.archive.$ts)"
                    (( updated += 1 ))
                else
                    log_error "  [$label] Failed to recreate @ (archive @.archive.$ts was created)"
                    (( failed += 1 ))
                fi
            else
                log_info "  [$label] @ exists, skipping (use --full to recreate)"
                (( skipped += 1 ))
            fi
        else
            if btrfs subvolume snapshot "$mnt/$latest_root" "$mnt/@"; then
                log_info "  [$label] Created @ from $latest_root"
                (( updated += 1 ))
            else
                log_error "  [$label] Failed to create @"
                (( failed += 1 ))
            fi
        fi

        # Update @home subvolume (archive-then-swap — see @ handling above for
        # the archive-before-delete rationale).
        if btrfs subvolume show "$mnt/@home" &>/dev/null; then
            if [[ "$force" == "true" ]]; then
                if ! btrfs subvolume snapshot -r "$mnt/@home" "$mnt/@home.archive.$ts"; then
                    log_error "  [$label] Failed to archive @home -> @home.archive.$ts — skipping recreation (old @home preserved)"
                    (( failed += 1 ))
                elif btrfs subvolume snapshot "$mnt/$latest_home" "$mnt/@home.new" && \
                     btrfs subvolume delete "$mnt/@home" && \
                     mv "$mnt/@home.new" "$mnt/@home"; then
                    log_info "  [$label] Recreated @home from $latest_home (archived old @home -> @home.archive.$ts)"
                    (( updated += 1 ))
                else
                    log_error "  [$label] Failed to recreate @home (archive @home.archive.$ts was created)"
                    (( failed += 1 ))
                fi
            else
                log_info "  [$label] @home exists, skipping (use --full to recreate)"
                (( skipped += 1 ))
            fi
        else
            if btrfs subvolume snapshot "$mnt/$latest_home" "$mnt/@home"; then
                log_info "  [$label] Created @home from $latest_home"
                (( updated += 1 ))
            else
                log_error "  [$label] Failed to create @home"
                (( failed += 1 ))
            fi
        fi
    done

    if (( failed > 0 )); then
        record_op "boot_subvols" "FAIL" "$updated updated, $failed failed"
    else
        record_op "boot_subvols" "OK" "$updated updated, $skipped skipped"
    fi
}

# Prune expired boot-subvolume archives (@.archive.<TS> / @home.archive.<TS>)
# created by update_boot_subvolumes(). Must run while targets are still
# mounted — boot-archive-cleanup.sh silently skips any target mount point
# that isn't currently mounted. Soft-fail: a pruner failure is recorded for
# the email report but never aborts the backup, matching run_indexer().
run_archive_cleanup() {
    local mode="$1"
    local args=()

    if [[ "$mode" == "dryrun" ]]; then
        args+=(--dryrun)
    fi

    if [[ ! -x "$BOOT_ARCHIVE_CLEANUP_BIN" ]]; then
        log_warn "Boot archive cleanup script not found at $BOOT_ARCHIVE_CLEANUP_BIN — skipping"
        record_op "archive_cleanup" "FAIL" "script not found at $BOOT_ARCHIVE_CLEANUP_BIN"
        return
    fi

    log_info "Running boot archive cleanup..."
    local cleanup_output
    if cleanup_output=$("$BOOT_ARCHIVE_CLEANUP_BIN" "${args[@]}" 2>&1); then
        # The detail used to be `tail -1` of human output, which is the
        # CONSTANT string "Boot archive cleanup complete." -- a status field
        # whose value never varies is not a status field. Require a real
        # per-target summary line to be present before calling this OK.
        # bd nsp (c10).
        #
        # The summary has one shape and this mode's own verb: a real run says
        # "Deleted N, kept N, errors N", a dry run "Would delete N, kept N,
        # errors N". Only the first was ever looked for, and the pruner's dry
        # run printed another line, so every dry run said "no per-target
        # summary" and was a FAIL (bd DAS-Backup-Manager-zwr). Each mode still
        # accepts only its own: a real run that printed "Would delete" ran
        # dry, and a dry run that printed "Deleted" ran for real, and
        # neither did what it was asked to.
        local cleanup_summary summary_form="Deleted N, kept N, errors N"
        local summary_re='Deleted [[:digit:]]+, kept [[:digit:]]+, errors [[:digit:]]+'
        if [[ "$mode" == "dryrun" ]]; then
            summary_form="Would delete N, kept N, errors N"
            summary_re='Would delete [[:digit:]]+, kept [[:digit:]]+, errors [[:digit:]]+'
        fi
        cleanup_summary=$(printf '%s\n' "$cleanup_output" \
            | grep -oE "$summary_re" | tr '\n' '; ') || true
        if [[ -z "$cleanup_summary" ]]; then
            log_warn "Boot archive cleanup exited 0 but printed no per-target summary ($summary_form) — treating as FAIL"
            record_op "archive_cleanup" "FAIL" "exit 0 with no summary line; pruner may have examined nothing"
        else
            record_op "archive_cleanup" "OK" "${cleanup_summary%; }"
            log_info "Boot archive cleanup completed"
        fi
    else
        local exit_code=$?
        log_warn "Boot archive cleanup failed (non-fatal): $cleanup_output"
        record_op "archive_cleanup" "FAIL" "exit code $exit_code"
    fi
}

# Whether <path> is a mount point, as mountpoint(1) answers it, with its two
# kinds of "no" kept apart. util-linux exits 0 for a mount point, 32 for a
# path that is not one, and 1 for a usage, permission or system error — and
# 1 as well for a path that does not exist (measured, util-linux 2.42.4).
# Returns 0 mounted, 1 not mounted, 2 could not tell, and for "could not
# tell" prints the probe's own message. A path that does not exist is not
# mounted (nothing can be mounted where there is nothing), and an
# unavailable target's mount point is removed on purpose (create_mount_points),
# so it is told from an error by mountpoint's message, read in the C locale;
# any other answer is "could not tell". Read as "not mounted", an error left
# the run's own source mount behind with nothing said (bd
# DAS-Backup-Manager-8cf, review F1), and passed the target unmount gate
# while a drive was mounted, so the run said the DAS could be disconnected
# (bd DAS-Backup-Manager-jug6).
probe_mount_point() { # probe_mount_point <path>
    local rc=0 err
    err="$(LC_ALL=C mountpoint "$1" 2>&1 >/dev/null)" || rc=$?
    case "$rc" in
        0) return 0 ;;
        32) return 1 ;;
    esac
    if ((rc == 1)) && [[ "$err" == *": No such file or directory" ]]; then
        return 1
    fi
    err="${err//$'\n'/; }"
    printf '%s (exit %s)\n' "${err:-mountpoint printed nothing}" "$rc"
    return 2
}

# The same budget as the Rust path (mount.rs UMOUNT_ATTEMPTS / _RETRY_PAUSE).
UMOUNT_ATTEMPTS=5
UMOUNT_RETRY_PAUSE=2

# umount one DAS target, retrying while it is busy. Returns 0 once it is
# unmounted, 1 after the last attempt failed; every failed attempt is logged
# with umount's own message.
umount_with_retry() {
    local mnt="$1" attempt err
    for (( attempt = 1; attempt <= UMOUNT_ATTEMPTS; attempt++ )); do
        if err="$(umount "$mnt" 2>&1)"; then
            return 0
        fi
        if (( attempt < UMOUNT_ATTEMPTS )); then
            log_warn "  umount $mnt failed (attempt $attempt of $UMOUNT_ATTEMPTS): ${err:-no message} — retrying in ${UMOUNT_RETRY_PAUSE}s"
            sleep "$UMOUNT_RETRY_PAUSE"
        else
            log_error "  umount $mnt failed (attempt $attempt of $UMOUNT_ATTEMPTS): ${err:-no message}"
        fi
    done
    return 1
}

unmount_all() {
    log_info "Unmounting volumes..."

    local -a still_mounted=() not_known=()

    # Unmount DAS backup targets in reverse order (0-based indexing). Every
    # configured mountpoint is attempted regardless of an earlier failure (a
    # stuck unmount must not mask the rest). A path the probe says is not a
    # mount point, or that is not there at all (an absent target's, removed
    # by create_mount_points), is skipped, not counted as a failure — the same
    # filesystem may legitimately be mounted elsewhere too (e.g. udisks under
    # /run/media), only this script's own mountpoints matter here. These
    # target mountpoints are the only thing that gates the "safe to
    # disconnect" claim in main() — they are the physical DAS enclosure.
    #
    # So the gate passes only on knowledge: a target the probe cannot tell
    # about FAILS it, whatever happens next — the run then says it is NOT
    # safe to disconnect, and why. The unmount is tried anyway, so whatever
    # was mounted there is taken down if it can be, and the detail says what
    # umount did. A probe error used to read as "not mounted": the target
    # was skipped, the gate passed, and the run said the DAS could be
    # disconnected while a drive was mounted (bd DAS-Backup-Manager-jug6).
    local mnt target_rc target_why
    for (( i=${#ALL_TARGET_MOUNTS[@]}-1; i>=0; i-- )); do
        mnt="${ALL_TARGET_MOUNTS[$i]}"
        target_rc=0
        target_why="$(probe_mount_point "$mnt")" || target_rc=$?
        case "$target_rc" in
            0)
                if ! umount_with_retry "$mnt"; then
                    log_error "  Failed to unmount $mnt"
                    still_mounted+=("$mnt")
                fi
                ;;
            1) ;;
            *)
                log_error "  Could not tell whether $mnt is mounted — $target_why; unmounting it anyway"
                if umount_with_retry "$mnt"; then
                    not_known+=("could not tell whether $mnt is mounted — $target_why; umount then succeeded")
                else
                    not_known+=("could not tell whether $mnt is mounted — $target_why; umount failed too")
                fi
                ;;
        esac
    done

    # Sources: only the mount points this run mounted itself
    # (SOURCE_MOUNTS_OWNED, recorded by mount_sources), last mounted first,
    # each once — however many sources share one. A source volume that was
    # already mounted when the run looked (fstab mounts /dasRaid0 and the
    # /.btrfs-* top levels) was used as found and is never unmounted here: the
    # run reads from it and owns nothing about it (bd DAS-Backup-Manager-8cf).
    # One unmounted is struck off, so a second pass — cleanup() after an
    # abort that follows main()'s own call — neither repeats it nor touches a
    # mount someone else has made there since. One the probe says is not
    # mounted is struck off as well, untouched, and said. One the probe
    # cannot tell about is said, with the probe's message, and unmounted
    # anyway: umount's own answer decides (8cf review, F1).
    #
    # Best effort, deliberately NOT tracked via record_op and NOT part of the
    # disconnect gate above: sources are host filesystems, not the removable
    # DAS enclosure, so one left mounted has no bearing on whether the DAS is
    # safe to disconnect. A helper mount that will not unmount is a WARN with
    # umount's own message (which used to be discarded), is left as it is,
    # and stays the run's, for a later pass to try again.
    local -a owned=("${SOURCE_MOUNTS_OWNED[@]}")
    local src_mnt umount_err probe_rc probe_why left
    for (( i=${#owned[@]}-1; i>=0; i-- )); do
        src_mnt="${owned[$i]}"
        probe_rc=0
        probe_why="$(probe_mount_point "$src_mnt")" || probe_rc=$?
        if ((probe_rc == 1)); then
            disown_source_mount "$src_mnt"
            log_info "  Source volume $src_mnt, recorded as this run's, is not mounted now — nothing to unmount"
            continue
        fi
        left="left mounted"
        if ((probe_rc != 0)); then
            log_warn "  Could not tell whether source volume $src_mnt, which this run mounted, is still mounted — $probe_why; unmounting it anyway"
            left="left as it is"
        fi
        if umount_err="$(umount "$src_mnt" 2>&1)"; then
            disown_source_mount "$src_mnt"
            log_info "  Unmounted source volume $src_mnt (this run mounted it)"
        else
            umount_err="${umount_err//$'\n'/; }"
            log_warn "  Could not unmount source volume $src_mnt, which this run mounted: ${umount_err:-no message} — $left; best effort, not a DAS disconnect concern"
        fi
    done

    # The detail says why, for each target not known to be released: still
    # mounted, or the probe could not tell. The report shows it beside
    # "Unmount targets", the history row in its errors, and main() after
    # "NOT safe to disconnect".
    if (( ${#still_mounted[@]} + ${#not_known[@]} > 0 )); then
        local detail="" item
        if (( ${#still_mounted[@]} > 0 )); then
            detail="still mounted: ${still_mounted[0]}"
            for item in "${still_mounted[@]:1}"; do
                detail+=", $item"
            done
        fi
        for item in "${not_known[@]}"; do
            detail+="${detail:+; }$item"
        done
        record_op "unmount" "FAIL" "$detail"
        log_error "Unmount FAILED — $detail"
    else
        record_op "unmount" "OK"
        log_info "All backup targets unmounted"
    fi
}

show_stats() {
    log_info "Backup statistics:"
    btrbk -c "$DAS_BTRBK_CONF" list latest 2>/dev/null || true

    log_info "Target disk usage:"
    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if mountpoint -q "$mnt" 2>/dev/null; then
            df -h "$mnt" 2>/dev/null || true
        fi
    done
}

# Get used bytes on a mounted filesystem
get_used_bytes() {
    df --output=used -B1 "$1" 2>/dev/null | tail -1 | tr -d ' '
}

# Format byte count to human-readable string
format_bytes() {
    local bytes=$1
    if (( bytes >= 1073741824 )); then
        awk "BEGIN {printf \"%.2f GiB\", $bytes / 1073741824}"
    elif (( bytes >= 1048576 )); then
        awk "BEGIN {printf \"%.2f MiB\", $bytes / 1048576}"
    elif (( bytes >= 1024 )); then
        awk "BEGIN {printf \"%.2f KiB\", $bytes / 1024}"
    else
        printf "%d B" "$bytes"
    fi
}

# Capture disk usage on all mounted backup targets
capture_usage() {
    local phase="$1"  # "before" or "after"

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if mountpoint -q "$mnt" 2>/dev/null; then
            local used
            used=$(get_used_bytes "$mnt")
            if [[ "$phase" == "before" ]]; then
                USAGE_BEFORE[$mnt]=$used
            else
                USAGE_AFTER[$mnt]=$used
            fi
        fi
    done

    if [[ "$phase" == "before" ]]; then
        BTRBK_START_TIME=$(date +%s)
    else
        BTRBK_END_TIME=$(date +%s)
    fi
}

# Cache all report data that requires a live mount or a live btrbk call,
# while targets are still mounted. MUST run before unmount_all() in main()'s
# real-backup path. generate_capacity_section(), generate_growth_section(),
# and the LATEST SNAPSHOTS line in generate_report() read only the globals
# populated here (REPORT_TARGETS, CAPACITY_*, AVAIL_BYTES, BTRBK_LATEST) —
# no mountpoint/df/btrbk calls after this point. Tracks bd DAS-Backup-Manager-ecg.
capture_report_data() {
    REPORT_TARGETS=()

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if ! mountpoint -q "$mnt" 2>/dev/null; then
            continue
        fi
        REPORT_TARGETS+=("$mnt")

        # `|| true` on both df calls: same set -euo pipefail hazard as
        # BTRBK_LATEST below, but worse here — capture_report_data() runs as
        # a bare statement in main() (not inside an if/&&/||), and this
        # script has no `set -o errtrace`, so a killed df here would NOT
        # even trigger the ERR trap/cleanup() — unmount_all() would simply
        # never run, leaving the DAS mounted with no email and no DB row.
        # A guarded df failure instead leaves df_line/the avail read empty;
        # the target stays in REPORT_TARGETS (mountpoint -q already
        # confirmed it's genuinely mounted) and renders as "?"/0 downstream
        # via the existing ${CAPACITY_*[$mnt]:-?} / ${AVAIL_BYTES[$mnt]:-0}
        # defaults in generate_capacity_section()/generate_growth_section()
        # — chosen over silently dropping the target so a genuine target
        # with a flaky df call still shows up in the report instead of
        # vanishing from it. Tracks bd DAS-Backup-Manager-ecg.
        local df_line
        df_line=$(df -h "$mnt" 2>/dev/null | tail -1) || true
        CAPACITY_USED[$mnt]=$(echo "$df_line" | awk '{print $3}')
        CAPACITY_AVAIL[$mnt]=$(echo "$df_line" | awk '{print $4}')
        CAPACITY_PCT[$mnt]=$(echo "$df_line" | awk '{print $5}')

        AVAIL_BYTES[$mnt]=$(df --output=avail -B1 "$mnt" 2>/dev/null | tail -1 | tr -d ' ') || true
    done

    # `|| true` is required, not decorative: under set -euo pipefail, a bare
    # `VAR=$(cmd1 | cmd2)` assignment fails the whole script the instant
    # `btrbk list latest` returns nonzero (config error, transient lock,
    # etc.) — the exact class of footgun run_btrbk()'s dryrun branch was
    # fixed for in v4.3.0 (bd DAS-Backup-Manager-vuw). Falling back to an
    # empty BTRBK_LATEST is safe: generate_report()'s
    # ${BTRBK_LATEST:-  (none yet)} already handles empty. Caught live by
    # the bd DAS-Backup-Manager-ecg verification harness, which reproduced
    # exactly this abort against a deliberately-broken btrbk config.
    #
    # A FAILED listing is not an empty one: the report says it is unavailable
    # and why, instead of "(none yet)" (bd DAS-Backup-Manager-h4t).
    local latest_err
    latest_err="$(mktemp)"
    if ! BTRBK_LATEST=$(btrbk -c "$DAS_BTRBK_CONF" list latest 2>"$latest_err" | awk 'NR>1{printf "  %s\n", $0}'); then
        BTRBK_LATEST="  (unavailable: btrbk list latest failed: $(head -n1 "$latest_err"))"
    fi
    rm -f "$latest_err"

    # Machine-readable copy for the DB counters. The human table's STATUS column
    # is presentation, not API: btrbk 0.32.7 renders it as "-" for every row and
    # the "up-to-date" string the counters grepped for no longer appears at all,
    # which silently zeroed snaps_created/snaps_sent on every run from
    # 2026-06-26 onward (bd DAS-Backup-Manager-oi0). `--format=raw` emits named
    # key='value' fields instead, so the counters no longer depend on how btrbk
    # chooses to lay out a table.
    #
    # The status is captured rather than discarded: an empty RAW because the
    # command FAILED and an empty RAW because nothing was sent both used to
    # collapse into "0 created, 0 sent" alongside a --success DB row.
    # bd DAS-Backup-Manager-nsp (c4).
    local raw_err
    raw_err="$(mktemp)"
    if BTRBK_LATEST_RAW=$(btrbk -c "$DAS_BTRBK_CONF" --format=raw list latest 2>"$raw_err"); then
        BTRBK_LATEST_RAW_OK="true"
    else
        BTRBK_LATEST_RAW_OK="false"
        BTRBK_LATEST_RAW=""
        log_warn "btrbk list latest failed — snapshot counters are UNKNOWN, not zero: $(tr '\n' ' ' <"$raw_err")"
    fi
    rm -f "$raw_err"
}

# Log throughput report for all targets
log_throughput() {
    local elapsed=$(( BTRBK_END_TIME - BTRBK_START_TIME ))
    local elapsed_min=$(( elapsed / 60 ))
    local elapsed_sec=$(( elapsed % 60 ))

    log_info "=== Throughput Report ==="
    log_info "  Elapsed: ${elapsed_min}m ${elapsed_sec}s"

    local total_written=0

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        local name="${TARGET_NAMES[$mnt]:-$mnt}"
        local before="${USAGE_BEFORE[$mnt]:-}"
        local after="${USAGE_AFTER[$mnt]:-}"

        if [[ -z "$before" || -z "$after" ]]; then
            continue
        fi

        local delta=$(( after - before ))
        total_written=$(( total_written + delta ))

        if (( delta <= 0 )); then
            log_info "  $name: no new data written"
        elif (( elapsed > 0 )); then
            local rate_bytes=$(( delta / elapsed ))
            log_info "  $name: $(format_bytes $delta) @ $(format_bytes $rate_bytes)/s"
        else
            log_info "  $name: $(format_bytes $delta)"
        fi
    done

    if (( total_written > 0 && elapsed > 0 )); then
        local total_rate=$(( total_written / elapsed ))
        log_info "  ─────────────────────────────────────"
        log_info "  Total: $(format_bytes $total_written) @ $(format_bytes $total_rate)/s"

        # ---- Machine-readable history (bd DAS-Backup-Manager-6lr) -----------
        #
        # The human report above was a producer with no consumer: emitted every
        # run, read by nothing, compared across runs by nothing.
        #
        # It also records the negotiated USB link speed, and that field — NOT
        # the throughput — is the one that detects a degraded bus. Measured
        # over the nine days the DAS ran at 480 Mbit/s instead of 10 Gbit/s:
        #
        #     2026-08-28 degraded  40.52 GiB / 75m27s =  9.17 MiB/s
        #     2026-08-29 restored   8.59 GiB / 13m22s = 10.97 MiB/s
        #     2026-08-31 restored  13.00 GiB / 11m12s = 19.80 MiB/s
        #
        # 9.17 against 10.97 is inside ordinary run-to-run variation, so a
        # throughput trend alone would never have flagged it. The enclosure is
        # deliberately bound to usb-storage (BOT) rather than UAS by
        # /etc/modprobe.d/terramaster-no-uas.conf; BOT issues one command at a
        # time with no queuing, so above roughly Gen 1 the spindles and BOT are
        # the limit, not the bus. That quirk is a correct stability trade and
        # should stay — but it is exactly why link rate and transfer rate are
        # only loosely coupled here.
        local link_speed="unknown"
        local d
        for d in /sys/bus/usb/devices/*/; do
            [[ -f "$d/speed" && -f "$d/product" ]] || continue
            if [[ "$(cat "$d/product" 2>/dev/null)" == *TDAS* ]]; then
                link_speed="$(cat "$d/speed" 2>/dev/null || echo unknown)"
                break
            fi
        done

        if [[ "$link_speed" != "unknown" && "$link_speed" -lt 5000 ]]; then
            log_warn "  DAS USB link negotiated at ${link_speed} Mbit/s — expected 10000."
            log_warn "  Backups will still succeed, just slower. Reseat the cable (power the"
            log_warn "  enclosure down first — see docs/DAS-BAY-MAPPING.md)."
            record_op "usb_link" "FAIL" "negotiated ${link_speed} Mbit/s, expected 10000"
        fi

        if [[ -n "${DAS_THROUGHPUT_LOG:-}" ]]; then
            printf '{"ts":"%s","elapsed_s":%d,"bytes":%d,"bytes_per_s":%d,"usb_link_mbit_s":"%s"}\n' \
                "$(date -Is)" "$elapsed" "$total_written" "$total_rate" "$link_speed" \
                >> "$DAS_THROUGHPUT_LOG" \
                || log_warn "Could not append to throughput log $DAS_THROUGHPUT_LOG"
        fi
    fi
}

# ============================================================================
# GROWTH TRACKING
# ============================================================================

# Append current usage to the growth log for trend analysis
record_growth() {
    mkdir -p "$(dirname "$GROWTH_LOG")"
    local ts
    ts=$(date '+%Y-%m-%dT%H:%M:%S')

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if mountpoint -q "$mnt" 2>/dev/null; then
            local used
            used=$(get_used_bytes "$mnt")
            echo "$ts $mnt $used" >> "$GROWTH_LOG"
        fi
    done
}

# Compute average daily growth over a lookback period (returns bytes/day)
compute_growth_stats() {
    local mnt="$1"
    local current_used="$2"
    local lookback_days="$3"

    if [[ ! -f "$GROWTH_LOG" ]]; then
        echo "0"
        return
    fi

    local now
    now=$(date +%s)
    local cutoff=$(( now - (lookback_days * 86400) ))

    # Find the oldest entry for this mount within the lookback window
    local oldest_epoch=0 oldest_used=0
    local ts entry_mnt entry_used entry_epoch
    while IFS=' ' read -r ts entry_mnt entry_used; do
        [[ "$entry_mnt" != "$mnt" ]] && continue
        # Parse ISO timestamp to epoch
        entry_epoch=$(date -d "$ts" '+%s' 2>/dev/null || echo 0)
        if (( entry_epoch >= cutoff && oldest_epoch == 0 )); then
            oldest_epoch=$entry_epoch
            oldest_used=$entry_used
            break
        fi
    done < "$GROWTH_LOG"

    if (( oldest_epoch == 0 )); then
        echo "0"
        return
    fi

    local actual_days=$(( (now - oldest_epoch) / 86400 ))
    if (( actual_days <= 0 )); then
        echo "0"
        return
    fi

    local delta=$(( current_used - oldest_used ))
    if (( delta < 0 )); then delta=0; fi

    echo $(( delta / actual_days ))
}

# ============================================================================
# SMART SUMMARY
# ============================================================================

# Get quick SMART data for a drive (returns "health|temp|hours|serial")
get_smart_summary() {
    local dev="$1"
    local health temp hours serial

    health=$(smartctl -H "$dev" 2>/dev/null | grep -o "PASSED\|FAILED" || echo "UNKNOWN")
    temp=$(smartctl -A "$dev" 2>/dev/null | awk '/Temperature_Celsius/{print $10}' || echo "?")
    hours=$(smartctl -A "$dev" 2>/dev/null | awk '/Power_On_Hours/{print $10}' || echo "?")
    serial=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3}' || echo "?")

    echo "${health}|${temp}|${hours}|${serial}"
}

# ============================================================================
# CONTENT INDEXER
# ============================================================================

run_indexer() {
    if [[ ! -x "$BTRDASD_BIN" ]]; then
        log_warn "Content indexer not built -- skipping (build with: cargo build --release --manifest-path indexer/Cargo.toml)"
        record_op "indexer" "SKIP" "binary not found"
        return
    fi

    # Find the primary target mount for indexing
    local primary_mount=""
    for label in "${!TARGET_ROLES[@]}"; do
        if [[ "${TARGET_ROLES[$label]}" == "primary" ]]; then
            primary_mount="${TARGET_MOUNTS[$label]}"
            break
        fi
    done

    if [[ -z "$primary_mount" ]] || ! mountpoint -q "$primary_mount" 2>/dev/null; then
        log_warn "Primary target not mounted — skipping indexer"
        record_op "indexer" "SKIP" "primary target not mounted"
        return
    fi

    log_info "Running content indexer..."
    local indexer_output
    # `walk` takes the maintenance lock before it mounts anything, and this
    # run holds it — on fd 8, which walk inherits. Naming that descriptor
    # hands the hold down; without it walk would wait for this run, which
    # waits for walk (bd DAS-Backup-Manager-frb). walk verifies the
    # descriptor; it does not take the name on trust.
    if indexer_output=$(DAS_MAINTENANCE_LOCK_FD=8 "$BTRDASD_BIN" walk "$primary_mount" --db "$DAS_DB_PATH" 2>&1); then
        record_op "indexer" "OK"
        log_info "  $indexer_output"
    else
        local exit_code=$?
        log_warn "Content indexer failed (non-fatal)"
        record_op "indexer" "FAIL" "exit code $exit_code"
    fi
}

# ============================================================================
# EMAIL REPORT
# ============================================================================

generate_report() {
    # The host's name, here and in the ABORTED report, the mail subject and the
    # From name, is bash's own $HOSTNAME, never the `hostname` program: bash
    # sets it from gethostname() when the shell starts, so under `set -u` it is
    # always set and needs no default, and a host without inetutils (no
    # packaging declares it; CI's container has none) still has it. The short
    # name, for the From name, is everything before the first dot, as
    # `hostname -s` printed it (bd DAS-Backup-Manager-arv1).
    local timestamp
    timestamp=$(date '+%Y-%m-%d %H:%M')
    local overall_status="ALL OPERATIONS SUCCESSFUL"
    if any_op_is FAIL; then
        overall_status="FAILURES DETECTED"
    elif any_op_is WARN; then
        overall_status="COMPLETED WITH WARNINGS"
    fi

    local elapsed=$(( BTRBK_END_TIME - BTRBK_START_TIME ))
    local elapsed_min=$(( elapsed / 60 ))
    local elapsed_sec=$(( elapsed % 60 ))

    # Optional sections, built by concatenation (not $(...), which would strip
    # the trailing newline). Each present section is wrapped in blank lines;
    # with neither present the variable is empty and the heredoc keeps its one
    # blank line between the operations list and THROUGHPUT.
    local subvol_sections=""
    # First: a run missing from the history is shown by nothing else
    # (report_unrecorded_run, bd DAS-Backup-Manager-6wt).
    if [[ "${OP_STATUS[run_history]:-}" == "FAIL" ]]; then
        subvol_sections+=$'\n'"RUN HISTORY"
        subvol_sections+=$'\n'"  NOT RECORDED: this run is missing from the backup history (backup_runs);"
        subvol_sections+=$'\n'"  btrdasd backup report and the GUI show the run before it as the latest."
        subvol_sections+=$'\n'"  ${OP_STATUS[run_history_detail]:-}"$'\n'
    fi
    if [[ -n "$SUBVOL_SYNC_REPORT" ]]; then
        subvol_sections+=$'\n'"$SUBVOL_SYNC_REPORT"$'\n'
    fi
    if [[ -n "$SUBVOL_EXPIRE_REPORT" ]]; then
        subvol_sections+=$'\n'"$SUBVOL_EXPIRE_REPORT"$'\n'
    fi
    if [[ -n "$RECOVERY_OS_REPORT" ]]; then
        subvol_sections+=$'\n'"$RECOVERY_OS_REPORT"$'\n'
    fi

    # Build the report
    cat <<-REPORT
===============================================================
  DAS Backup Report — $timestamp
  Host: $HOSTNAME
  Status: $overall_status
===============================================================

BACKUP OPERATIONS
───────────────────────────────────────────────────────────────
  Maintenance lock      ${OP_STATUS[lock_wait]:-OK}  (${OP_STATUS[lock_wait_detail]:-no wait})
  btrbk send/receive    ${OP_STATUS[btrbk]:-N/A}  (${elapsed_min}m ${elapsed_sec}s)
  Snapshot counts       ${OP_STATUS[btrbk_counters]:-N/A}  (${OP_STATUS[btrbk_counters_detail]:-n/a})
  Boot subvolumes       ${OP_STATUS[boot_subvols]:-N/A}  (${OP_STATUS[boot_subvols_detail]:-n/a})
  Archive cleanup       ${OP_STATUS[archive_cleanup]:-N/A}  (${OP_STATUS[archive_cleanup_detail]:-n/a})
  Unmount targets       ${OP_STATUS[unmount]:-N/A}  (${OP_STATUS[unmount_detail]:-all clean})
  Content indexer        ${OP_STATUS[indexer]:-N/A}  (${OP_STATUS[indexer_detail]:-n/a})
  Subvolume sync        ${OP_STATUS[subvol_sync]:-N/A}  (${OP_STATUS[subvol_sync_detail]:-n/a})
  Retired expiry        ${OP_STATUS[subvol_expire]:-N/A}  (${OP_STATUS[subvol_expire_detail]:-n/a})
  Recovery OS           ${OP_STATUS[recovery_os]:-N/A}  (${OP_STATUS[recovery_os_detail]:-n/a})
${subvol_sections}
THROUGHPUT
───────────────────────────────────────────────────────────────
$(generate_throughput_section)

DISK CAPACITY
───────────────────────────────────────────────────────────────
$(generate_capacity_section)

GROWTH ANALYSIS
───────────────────────────────────────────────────────────────
$(generate_growth_section)

SMART STATUS
───────────────────────────────────────────────────────────────
$(generate_smart_section)

LATEST SNAPSHOTS
───────────────────────────────────────────────────────────────
${BTRBK_LATEST:-  (none yet)}

===============================================================
  backup-run.sh v4.11.3
  Next scheduled: $(systemctl show das-backup.timer --property=NextElapseUSecRealtime 2>/dev/null | cut -d= -f2 | sed 's/ [A-Z]*$//' || echo "unknown")
===============================================================
REPORT
}

generate_throughput_section() {
    local elapsed=$(( BTRBK_END_TIME - BTRBK_START_TIME ))
    local total_written=0

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        local name="${TARGET_NAMES[$mnt]:-$mnt}"
        local before="${USAGE_BEFORE[$mnt]:-}"
        local after="${USAGE_AFTER[$mnt]:-}"

        if [[ -z "$before" || -z "$after" ]]; then continue; fi

        local delta=$(( after - before ))
        total_written=$(( total_written + delta ))

        if (( delta <= 0 )); then
            printf "  %-24s no new data\n" "$name"
        elif (( elapsed > 0 )); then
            local rate=$(( delta / elapsed ))
            printf "  %-24s %s @ %s/s\n" "$name" "$(format_bytes $delta)" "$(format_bytes $rate)"
        else
            printf "  %-24s %s\n" "$name" "$(format_bytes $delta)"
        fi
    done

    if (( total_written > 0 && elapsed > 0 )); then
        local total_rate=$(( total_written / elapsed ))
        printf "  ─────────────────────────────────────\n"
        printf "  %-24s %s @ %s/s\n" "Total" "$(format_bytes $total_written)" "$(format_bytes $total_rate)"
    fi
}

generate_capacity_section() {
    printf "  %-24s %-10s %-10s %s\n" "Target" "Used" "Avail" "Use%"

    # Reads ONLY the cache populated by capture_report_data() while targets
    # were still mounted — no live mountpoint/df calls here. See
    # capture_report_data() for why: this runs after unmount_all() in
    # main()'s real-backup path. Tracks bd DAS-Backup-Manager-ecg.
    for mnt in "${REPORT_TARGETS[@]}"; do
        local name="${TARGET_NAMES[$mnt]:-$mnt}"
        printf "  %-24s %-10s %-10s %s\n" "$name" "${CAPACITY_USED[$mnt]:-?}" "${CAPACITY_AVAIL[$mnt]:-?}" "${CAPACITY_PCT[$mnt]:-?}"
    done
}

generate_growth_section() {
    # Reads ONLY the cache populated by capture_report_data() — see
    # generate_capacity_section() above. Tracks bd DAS-Backup-Manager-ecg.
    for mnt in "${REPORT_TARGETS[@]}"; do
        local name="${TARGET_NAMES[$mnt]:-$mnt}"
        local current="${USAGE_AFTER[$mnt]:-0}"
        local today_delta=$(( ${USAGE_AFTER[$mnt]:-0} - ${USAGE_BEFORE[$mnt]:-0} ))

        printf "  %s:\n" "$name"
        printf "    Today:              +%s\n" "$(format_bytes $today_delta)"

        local avg_7d avg_30d
        avg_7d=$(compute_growth_stats "$mnt" "$current" 7)
        avg_30d=$(compute_growth_stats "$mnt" "$current" 30)

        if (( avg_7d > 0 )); then
            printf "    7-day avg:          %s/day\n" "$(format_bytes "$avg_7d")"
        fi
        if (( avg_30d > 0 )); then
            printf "    30-day avg:         %s/day\n" "$(format_bytes "$avg_30d")"
        fi

        # Capacity runway projection (cached avail bytes, captured while mounted)
        local avail_bytes="${AVAIL_BYTES[$mnt]:-0}"
        local growth_rate=$avg_30d
        if (( growth_rate <= 0 )); then growth_rate=$avg_7d; fi
        if (( growth_rate <= 0 && today_delta > 0 )); then growth_rate=$today_delta; fi

        if (( growth_rate > 0 && avail_bytes > 0 )); then
            local days_left=$(( avail_bytes / growth_rate ))
            local years_left
            years_left=$(awk "BEGIN {printf \"%.1f\", $days_left / 365}")
            printf "    Capacity runway:    ~%s days (~%s years)\n" "$days_left" "$years_left"
        else
            printf "    Capacity runway:    no growth trend yet\n"
        fi
        echo ""
    done
}

generate_smart_section() {
    for label in "${!DISCOVERED_DEVICES[@]}"; do
        local drive="${DISCOVERED_DEVICES[$label]}"
        if [[ -z "$drive" || ! -b "$drive" ]]; then continue; fi
        local name="${TARGET_NAMES[${TARGET_MOUNTS[$label]}]:-$label}"
        local smart_data
        smart_data=$(get_smart_summary "$drive")
        local health=${smart_data%%|*}; smart_data=${smart_data#*|}
        local temp=${smart_data%%|*}; smart_data=${smart_data#*|}
        local hours=${smart_data%%|*}; smart_data=${smart_data#*|}
        local serial=$smart_data
        printf "  %-24s %-10s %-8s %s°C  %s hours\n" "$name" "$serial" "$health" "$temp" "$hours"
    done
}

# Upper bound on one report's delivery. mailx talks SMTP to the local relay,
# which takes a report in well under a second. s-nail gives up by itself
# after about 45 s of silence on a read (measured, s-nail 14.9.25: 44 s
# against a relay that accepts and never speaks, 44 s against one that
# greets and stalls; exit 4), but not on a relay that keeps trickling bytes.
# That one held the run for as long as it trickled: the DAS mounted (an
# abort sends before it unmounts), both locks held, a scrub waiting behind
# them, and the alert the very thing stuck (bd DAS-Backup-Manager-d1r, round
# 3: M1; the 45 s measured in round 4: N2). The bound covers the trickle and
# any helper mailx starts: TERM after MAIL_TIMEOUT_SECS, KILL
# MAIL_KILL_AFTER_SECS later, as the recovery OS check bounds its reads.
MAIL_TIMEOUT_SECS=60 MAIL_KILL_AFTER_SECS=10

# Where the last report is, for each line saying it was not emailed: the file
# when send_report() could write it, otherwise the journal, which every caller
# echoes the report to before it calls send_report().
report_whereabouts() {
    if [[ "$REPORT_SAVED" == "true" ]]; then
        echo "it is in $LAST_REPORT"
    else
        echo "it could not be saved to $LAST_REPORT either: it is in the journal only"
    fi
}

send_report() {
    local report="$1"
    local overall_status="$2"

    # Always saved to a file first, BEFORE any send attempt, so the report
    # survives a relay outage: a failed send loses nothing. A save that fails
    # (a full disk, a directory where the file goes) is said, not assumed —
    # the log used to say "Report saved" either way, and the lines below "it
    # is in $LAST_REPORT" (bd DAS-Backup-Manager-d1r, round 3: M3).
    REPORT_SAVED="false"
    if mkdir -p "$(dirname "$LAST_REPORT")" && printf '%s\n' "$report" >"$LAST_REPORT"; then
        REPORT_SAVED="true"
        log_info "Report saved to $LAST_REPORT"
    else
        log_error "Could not save the report to $LAST_REPORT — it is in the journal only"
    fi

    if [[ "${DAS_EMAIL_ENABLED:-false}" != "true" ]]; then
        if [[ "$REPORT_SAVED" == "true" ]]; then
            log_info "Email reporting disabled in config — not emailed; $(report_whereabouts)"
            return 0
        fi
        # Saved nowhere and sent nowhere: the journal has the only copy. The
        # operator's rule (3: the run began and something failed) makes that
        # a failure, and the next run would meet the same full disk; it used
        # to end in exit 0 and a success row (bd DAS-Backup-Manager-d1r,
        # round 4: N4). A report failure, not an email one: email is off.
        log_error "Email reporting disabled in config, and the report could not be saved: it is in the journal only"
        record_op "report" "FAIL" "not saved to $LAST_REPORT, and email is disabled: the journal has the only copy"
        return 1
    fi

    # Relay coordinates come from config via `btrdasd config dump-env`. Before
    # 2026-08-06 these exports existed and nothing read them; the script parsed
    # Protonmail Bridge credentials instead, so editing [email] in config.toml
    # had no effect on where mail went.
    local smtp_url="smtp://${DAS_EMAIL_SMTP_HOST}:${DAS_EMAIL_SMTP_PORT}"
    local report_to="${DAS_REPORT_TO:-$DAS_EMAIL_TO}"
    local report_from_addr="${DAS_REPORT_FROM:-$DAS_EMAIL_FROM}"

    if [[ -z "$report_to" || -z "$report_from_addr" ]]; then
        log_warn "Email enabled but from/to unset in config — not emailed; $(report_whereabouts)"
        return 1
    fi

    # A bare address gets the standard display name so reports stand out in the
    # inbox From column; an override that already has one is used verbatim.
    # s-nail extracts the bracketed address for the SMTP envelope, and the
    # envelope sender is what the relay keys its upstream credential on.
    local report_from="$report_from_addr"
    if [[ "$report_from_addr" != *"<"* ]]; then
        report_from="DAS Backup (${HOSTNAME%%.*}) <${report_from_addr}>"
    fi

    local subject
    subject="[DAS Backup] $HOSTNAME — $overall_status — $(date '+%Y-%m-%d %H:%M')"

    # Submit to the local relay: no credentials, no TLS on this hop. The relay
    # owns the authenticated, certificate-verified leg to the provider.
    #   smtp-auth=none  — REQUIRED. s-nail defaults to demanding a password for
    #                     any smtp:// mta and aborts with exit 4 without this.
    #   nosave          — a failed send otherwise drops the body in
    #                     /root/dead.letter, which nothing ever reads or prunes.
    #                     The report is already in $LAST_REPORT, or in the
    #                     journal if that write failed.
    #
    # stderr is captured rather than discarded: a successful send emits nothing
    # on stderr (measured), so anything here is the real reason for a failure.
    # The previous `2>/dev/null` claimed to hide s-nail deprecation warnings
    # that do not exist, and cost every failure its diagnosis.
    #
    # Bounded (MAIL_TIMEOUT_SECS above): timeout puts mailx in a process
    # group of its own and signals the whole group. mailx runs with fds 8 and
    # 9 closed, so nothing it starts can keep this run's locks once the run
    # ends. The report goes in as a here-string, not through a pipe.
    local mail_err rc=0
    mail_err=$(timeout -k "$MAIL_KILL_AFTER_SECS" "$MAIL_TIMEOUT_SECS" mailx \
        -s "$subject" \
        -r "$report_from" \
        -S v15-compat \
        -S "mta=${smtp_url}" \
        -S "smtp-auth=none" \
        -S nosave \
        "$report_to" 8>&- 9>&- <<<"$report" 2>&1 >/dev/null) || rc=$?

    if [[ $rc -eq 0 ]]; then
        log_info "Report emailed to $report_to via $smtp_url"
        [[ -n "$mail_err" ]] && log_warn "mailx wrote to stderr despite success: $mail_err"
        return 0
    fi
    # timeout: 124 after the TERM, 137 if it took the KILL.
    if ((rc == 124 || rc == 137)); then
        log_warn "Sending to $smtp_url did not finish within $MAIL_TIMEOUT_SECS s — gave up emailing the report to $report_to — $(report_whereabouts)"
    else
        log_warn "Failed to email report to $report_to via $smtp_url (mailx exit $rc) — $(report_whereabouts)"
    fi
    [[ -n "$mail_err" ]] && log_warn "mailx: $mail_err"
    return 1
}

# ============================================================================
# RECORD BACKUP RUN IN DATABASE
# ============================================================================

# The snapshot counts this run reports, into RUN_COUNTS, from the
# `btrbk --format=raw list latest` output capture_report_data() cached while
# the targets were still mounted (after unmount_all a live listing shows every
# target's STATUS as `-`, bd DAS-Backup-Manager-ecg) — and their report row:
# btrbk_counters OK with the counts when they are known, FAIL when they are
# not. The row reads N/A until this has run, like every other row: its default
# used to be "OK (counted)", so a report built before the counts were decided
# would have read as a success (bd DAS-Backup-Manager-d1r, round 3: M6).
#
# main() calls this after capture_report_data and BEFORE run_status() and
# generate_report(): a counter failure must read FAILURES DETECTED in the
# report and be no --success in the history, as it makes the run exit 3. It
# was decided only inside record_run_args, after both were fixed, so the email
# said ALL OPERATIONS SUCCESSFUL (bd DAS-Backup-Manager-bzw). record_run_args
# calls it again for the vector — the same answer, and the warning once.
#
# A count that is not known is said as --counts-unknown and stored as NULL,
# never as a number: a 0 reads as "nothing was sent", and the -1 this used
# to send (bd nsp c4/c5) was refused by record-run's parser as an unknown
# option, so the run was not recorded at all. Unknown when the listing was
# never read (the run ended before capture_report_data, so its state is
# still the empty starting value), when it failed, and when its output held
# no field this parser knows (btrbk renamed its raw fields: bd oi0, 06p).
# A listing that succeeded with no output is a measured 0.
decide_run_counts() {
    RUN_COUNTS=(--counts-unknown)
    local snaps_created snaps_sent
    case "$BTRBK_LATEST_RAW_OK" in
        true)
            if [[ -z "$BTRBK_LATEST_RAW" ]]; then
                RUN_COUNTS=(--snaps-created 0 --snaps-sent 0)
                record_op "btrbk_counters" "OK" "0 created, 0 sent"
            else
                # One source subvolume yields one snapshot, replicated to N
                # targets, so the two counts are genuinely different numbers.
                snaps_created=$(printf '%s\n' "$BTRBK_LATEST_RAW" \
                    | grep -o "snapshot_subvolume='[^']*'" | sort -u | grep -c . || true)
                snaps_sent=$(printf '%s\n' "$BTRBK_LATEST_RAW" \
                    | grep -c "target_subvolume='[^']" || true)
                if (( snaps_created == 0 && snaps_sent == 0 )); then
                    if [[ "${OP_STATUS[btrbk_counters]:-}" != "FAIL" ]]; then
                        log_warn "btrbk produced output but no snapshot_subvolume/target_subvolume fields parsed —"
                        log_warn "  the --format=raw field names have probably changed. Counts recorded as unknown."
                    fi
                    record_op "btrbk_counters" "FAIL" "raw output present but no fields parsed; counts unknown"
                else
                    RUN_COUNTS=(--snaps-created "$snaps_created" --snaps-sent "$snaps_sent")
                    record_op "btrbk_counters" "OK" "$snaps_created created, $snaps_sent sent"
                fi
            fi
            ;;
        false)
            record_op "btrbk_counters" "FAIL" "btrbk list latest failed; counts unknown"
            ;;
        *)
            record_op "btrbk_counters" "FAIL" "not read — the run ended before the snapshot counters were taken"
            ;;
    esac
}

# The `btrdasd backup record-run` argument vector for this run, into
# RECORD_RUN_ARGS. Built apart from the call so that
# indexer/tests/record_run_contract.rs can hand this exact vector, built by this
# code, to the real binary: a stub that accepts anything never noticed that
# record-run refused `--snaps-created -1`, which kept every run whose counters
# could not be read — the failed ones — out of the history
# (bd DAS-Backup-Manager-6wt).
#
# $1 is the run status (SUCCESS or FAILURE), $2 "true" for a full run.
record_run_args() {
    local overall_status="$1"
    local force_full="$2"

    local mode="incremental"
    if [[ "$force_full" == "true" ]]; then
        mode="full"
    fi

    local elapsed=$(( BTRBK_END_TIME - BTRBK_START_TIME ))

    # Compute total bytes sent across all targets
    local total_bytes=0
    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        local before="${USAGE_BEFORE[$mnt]:-0}"
        local after="${USAGE_AFTER[$mnt]:-0}"
        local delta=$(( after - before ))
        if (( delta > 0 )); then
            total_bytes=$(( total_bytes + delta ))
        fi
    done

    # Snapshot counters: decided by decide_run_counts — already in main(),
    # before the run status and the report (bd DAS-Backup-Manager-bzw); again
    # here, with the same answer, for a run cleanup() records.
    decide_run_counts

    # Collect errors from failed operations (newline-separated for DB storage)
    local error_list=""
    for op in "${!OP_STATUS[@]}"; do
        if [[ "${OP_STATUS[$op]}" == "FAIL" ]]; then
            local detail="${OP_STATUS[${op}_detail]:-}"
            if [[ -n "$detail" ]]; then
                error_list="${error_list:+${error_list}$'\n'}${op}: ${detail}"
            else
                error_list="${error_list:+${error_list}$'\n'}${op} failed"
            fi
        fi
    done

    RECORD_RUN_ARGS=(
        backup record-run
        --db "$DAS_DB_PATH"
        --mode "$mode"
        "${RUN_COUNTS[@]}"
        --bytes-sent "$total_bytes"
        --duration-secs "$elapsed"
    )
    if [[ "$overall_status" == "SUCCESS" ]]; then
        RECORD_RUN_ARGS+=(--success)
    fi
    if [[ -n "$error_list" ]]; then
        RECORD_RUN_ARGS+=(--errors "$error_list")
    fi
}

record_backup_run_in_db() {
    # Guard against double-recording (normal path + cleanup trap)
    if [[ "$BACKUP_RUN_RECORDED" == "true" ]]; then
        return 0
    fi

    record_run_args "$1" "$2"

    # A run that is not recorded is missing from `btrdasd backup report`, the
    # GUI history and everything else that reads backup_runs, which then show
    # the run before it as the latest. That is a FAIL, not a warning: the run
    # status turns FAILURE and report_unrecorded_run says so in the report,
    # and the run exits 3 (EXIT STATUS).
    #
    # Marked attempted as soon as the attempt returns, before anything is
    # logged: a log line that fails ends the run under set -e, and cleanup()
    # would then record it a second time (bd DAS-Backup-Manager-2my).
    local record_err record_rc=0
    record_err=$("$BTRDASD_BIN" "${RECORD_RUN_ARGS[@]}" 2>&1) || record_rc=$?
    BACKUP_RUN_RECORDED="true"
    if ((record_rc == 0)); then
        log_info "Backup run recorded in database"
    else
        log_error "This run is NOT in the backup history — recording it failed: $record_err"
        record_op "run_history" "FAIL" "recording it failed: $(head -n1 <<<"$record_err")"
    fi
}

# Write and send the report again when the run could not be recorded. The
# report goes out first because the record carries its outcome (a delivery
# failure fails the run: bd nsp b15), so the copy already sent could not say
# that the record then failed — and with the run missing from the history, the
# report is the only place left that shows it. The new copy reads FAILURES
# DETECTED and carries the RUN HISTORY section (generate_report).
report_unrecorded_run() {
    if [[ "${OP_STATUS[run_history]:-}" != "FAIL" ]]; then
        return 0
    fi
    local report
    report=$(generate_report)
    echo ""
    echo "$report"
    if ! send_report "$report" "$(subject_status)"; then
        log_warn "The report saying this run is not recorded was not emailed — $(report_whereabouts)"
    fi
}

# ============================================================================
# EXIT STATUS — the rule and every path: EXIT STATUS in the header
# ============================================================================

# The status of a run that reached the end of main(): 3 when any operation
# FAILED, else 0 — a WARN (a stale recovery OS) is not a failure. The same
# test run_status() makes for the history row and generate_report() for its
# status line, on the operations recorded by then. Three FAILs can come after
# the report is built: email delivery (the history row then says FAILURE),
# the history record itself (report_unrecorded_run sends the report again
# saying so), and, with email off, a report that could not be saved either
# (send_report records it; the row says FAILURE and why, while the journal's
# only copy of the report still reads as it was built — round 4, N4). The
# snapshot counters used to come after it too; they are decided before the
# report now (decide_run_counts, bd DAS-Backup-Manager-bzw).
completed_exit_status() {
    if any_op_is FAIL; then
        echo 3
    else
        echo 0
    fi
}

# The status of a run that ends before main() completes — what cleanup()
# exits with. $1 is the status bash is exiting with, $2 "true" once the run
# holds the maintenance lock (CLEANUP_ARMED), $3 the signal that stopped the
# run (STOP_SIGNAL), or empty.
#   $1       a signal stopped it (HUP INT USR1 PIPE ALRM TERM): the code its
#            trap exited with, 128 + the signal's number, kept as it is.
#   3        it held the lock, so it had begun its work. Whatever stopped it —
#            an `exit 3`, or a command failing under set -e with its own
#            status (a guard's 3, a pipeline's SIGPIPE 141, a child killed
#            by TERM 143) — the run began and failed.
#   1        it had not: it could not start.
# Called only by cleanup(), the EXIT trap. shellcheck 0.11 stops seeing a
# trap's handlers as called once the last line ends in `exit` (SC2329).
# shellcheck disable=SC2329
abort_exit_status() {
    local rc="$1" armed="$2" signal="$3"
    if [[ -n "$signal" ]]; then
        echo "$rc"
    elif [[ "$armed" == "true" ]]; then
        echo 3
    else
        echo 1
    fi
}

# The operations that FAILED, sorted and comma-separated, for the line that
# says why a run exits 3.
failed_ops() {
    local op names
    names="$(for op in "${!OP_STATUS[@]}"; do
        if [[ "$op" != *_detail && "${OP_STATUS[$op]}" == "FAIL" ]]; then
            echo "$op"
        fi
    done | sort)"
    echo "${names//$'\n'/, }"
}

# ============================================================================
# ABORTED RUNS — one report and one history row (bd DAS-Backup-Manager-2my)
# ============================================================================
#
# A run that aborts with 3 before its report — no primary target, a target
# or source that fails verification, the bare-mountpoint guard, a command
# failing under set -e — exits with a status its units count as success. It
# sent no report and wrote no history row, so with no failed unit either it
# was silent: a powered-off DAS would have failed every night unseen.
# cleanup() now records it as failed and sends one ABORTED report through
# send_report's relay path, both best effort: their own failures are logged
# and change neither the status nor what the run did.

# Keep what stops the run ($1) and why ($2, one line per finding) for
# cleanup()'s report and history row. The caller has logged the detail, and
# exits 3 itself on the next line: a guard never relies on a helper to stop.
abort_reason() {
    ABORT_WHAT="$1"
    ABORT_REASON="$2"
}

# Make what stopped an aborted run an operation that FAILED —
# `aborted: <what>: <why>`, the reason's lines joined with "; " — so the run
# status, the history row's errors and the report all carry it. A command
# that failed under set -e, with no abort_reason before it, is named by its
# status ($1) and the call chain it failed in ($2: "log < log_info < main",
# innermost first) — the step. Never by $BASH_COMMAND: that is the command's
# unexpanded source text, `mount ... "$dev" "$mnt"`, which named neither the
# source nor the device (bd DAS-Backup-Manager-d1r, round 3: M4). Its own
# message, if it wrote one, is in the journal.
# Called only by cleanup(), the EXIT trap (SC2329: see clear_maintenance_holder).
# shellcheck disable=SC2329
note_abort() {
    local rc="$1" where="$2"
    if [[ -z "${ABORT_WHAT:-}" ]]; then
        ABORT_WHAT="a command that failed"
        ABORT_REASON="exit status $rc${where:+ in $where}"
    fi
    local reason="${ABORT_REASON:-}"
    record_op "aborted" "FAIL" "$ABORT_WHAT: ${reason//$'\n'/; }"
}

# The ABORTED report: what stopped the run and why, what was backed up,
# which targets it found, whether it is in the history, and where its log
# is. $1 is the status the run exits with. Called only by send_abort_report.
# shellcheck disable=SC2329
generate_abort_report() {
    local status="$1"
    local timestamp
    timestamp=$(date '+%Y-%m-%d %H:%M')

    # Nothing is backed up until btrbk starts (capture_usage stamps it).
    local backed_up
    if [[ -n "${OP_STATUS[btrbk]:-}" ]]; then
        backed_up="btrbk had finished (${OP_STATUS[btrbk]}${OP_STATUS[btrbk_detail]:+: ${OP_STATUS[btrbk_detail]}}); the run stopped after it"
    elif ((BTRBK_START_TIME != 0)); then
        backed_up="not known — btrbk was running when the run stopped"
    else
        backed_up="nothing — the run stopped before btrbk started"
    fi

    # The targets detection found and did not, once it had run.
    local seen="not known" unseen="not known" label
    if ((${#TARGET_AVAILABLE[@]} > 0)); then
        seen="" unseen=""
        while IFS= read -r label; do
            if [[ "${TARGET_AVAILABLE[$label]}" == "true" ]]; then
                seen+="${seen:+, }$label"
            else
                unseen+="${unseen:+, }$label"
            fi
        done < <(printf '%s\n' "${!TARGET_AVAILABLE[@]}" | sort)
        seen="${seen:-none}" unseen="${unseen:-none}"
    fi

    local history
    if [[ "${OP_STATUS[run_history]:-}" == "FAIL" ]]; then
        history="NOT recorded — ${OP_STATUS[run_history_detail]:-}"
    elif [[ "$BACKUP_RUN_RECORDED" == "true" ]]; then
        history="recorded as failed"
    else
        history="not recorded"
    fi

    # The reason's first line beside its label, any further ones below it.
    local why="${ABORT_REASON:-}" why_more=""
    if [[ "$why" == *$'\n'* ]]; then
        why_more="$(printf '%s\n' "${why#*$'\n'}" | sed 's/^/                     /')"$'\n'
        why="${why%%$'\n'*}"
    fi

    cat <<-REPORT
===============================================================
  DAS Backup Report — $timestamp
  Host: $HOSTNAME
  Status: ABORTED
===============================================================

The backup run aborted before its report and exited $status. Its units
count that as success, so this report, the history and the journal are
the record of it.

  What aborted:      ${ABORT_WHAT:-not known}
  Why:               $why
${why_more}  Backed up:         $backed_up
  Targets seen:      $seen
  Targets not seen:  $unseen
  History:           $history
  Log:               $LOG_FILE
===============================================================
REPORT
}

# Print and send the ABORTED report — through send_report, the relay path the
# full report takes, so $LAST_REPORT holds it before any send is tried and a
# relay outage costs delivery only. Called only by cleanup().
# shellcheck disable=SC2329
send_abort_report() {
    local report
    report="$(generate_abort_report "$1")"
    echo ""
    echo "$report"
    if ! send_report "$report" "ABORTED"; then
        log_warn "The report saying this run aborted was not emailed — $(report_whereabouts)"
    fi
}

# ============================================================================
# CLEANUP
# ============================================================================

# Run only through the EXIT trap. shellcheck 0.11 stops seeing a trap's
# handlers as called once the last line ends in `exit` (SC2329).
# shellcheck disable=SC2329
cleanup() {
    # First statement, unconditionally: capture the exit status that
    # triggered this EXIT trap invocation BEFORE any other command in this
    # function can overwrite $? — main()'s own status (0 or 3) when it
    # completed, or the status of whatever `exit N`, set -e abort or signal
    # trap fired the trap. FUNCNAME still holds the call chain the run was
    # in when the trap fired — for a set -e abort, where the failing command
    # ran (measured: the function it failed in is FUNCNAME[1]) — and is what
    # names the step (note_abort; round 3, M4).
    local rc=$? where="" fn
    # From the caller up, leaving out bash's own last entry, "main" for the
    # script's top level (this script's main() is the entry before it).
    for fn in "${FUNCNAME[@]:1:${#FUNCNAME[@]}-2}"; do
        where+="${where:+ < }$fn"
    done

    # Best effort from here on, and the exit below is the only status. Under
    # set -e a command failing inside an EXIT trap ends bash at once with
    # that command's own status (measured, bash 5.3): a log line this trap
    # could not write — the log file unwritable mid-run, which is often what
    # ended the run — made it exit 1 instead of the run's status and skip
    # the unmount (bd DAS-Backup-Manager-d1r).
    set +e
    # Nor may a stdout that went away: with SIGPIPE trapped (or at its
    # default) the first log line written to a gone stream would end this
    # trap there, before the unmount. Ignored from here on, such a write just
    # fails (bd DAS-Backup-Manager-d1r, round 3: M2).
    trap '' PIPE

    # The dry run's planned btrbk.conf goes on every exit path, before either
    # early return below.
    if [[ -n "${DRYRUN_BTRBK_CONF:-}" ]]; then
        rm -f -- "$DRYRUN_BTRBK_CONF"
    fi

    # SCRIPT_COMPLETED == "true": main() already ran unmount_all()/
    # record_backup_run_in_db() itself and reached its own last statement —
    # the ordinary completion path. Exit silently with main()'s status
    # (completed_exit_status), explicitly, so nothing here can alter it.
    #
    # Once the lock is this run's (CLEANUP_ARMED), its holder record is
    # emptied on every way out — here, and after the recovery body below —
    # while fd 8 still holds the lock (bd DAS-Backup-Manager-frb).
    if [[ "$SCRIPT_COMPLETED" == "true" ]]; then
        if [[ "$CLEANUP_ARMED" == "true" ]]; then
            clear_maintenance_holder
        fi
        exit "$rc"
    fi

    # main() did not complete. The status follows EXIT STATUS: a signal's
    # own code kept, else 1 if the run never held the maintenance lock and 3
    # if it did (bd DAS-Backup-Manager-d1r).
    local status
    status="$(abort_exit_status "$rc" "$CLEANUP_ARMED" "$STOP_SIGNAL")"

    # CLEANUP_ARMED != "true": this process has not yet reached the point
    # where it actually owns the shared DAS mountpoints (see CLEANUP_ARMED's
    # definition in the globals block and its arming site in main(), right
    # after acquire_maintenance_lock()). The EXIT trap is installed, and
    # ALL_TARGET_MOUNTS is already populated, well before main() is even
    # entered — so WITHOUT this gate, an abort as early as main()'s own
    # argument-parsing usage error (`exit 1` for an unrecognized flag) or a
    # check_root() failure would call unmount_all() over ALL_TARGET_MOUNTS
    # before this process holds /run/das-maintenance.lock. Those same
    # /mnt/backup-* paths are mounted by indexer/src/scrub.rs under that
    # identical lock's protection — scrub.rs's own doc comment states the
    # invariant this gate exists to preserve: "a backup can never unmount a
    # filesystem out from under a running scrub". Such a run could not
    # start, which is neither abnormal termination of a run in progress nor
    # anything to clean up: it exits silently, 1 (or a signal's code).
    # bd DAS-Backup-Manager-oeo.
    if [[ "$CLEANUP_ARMED" != "true" ]]; then
        exit "$status"
    fi

    log_warn "Cleaning up after abnormal termination (status $rc; exiting $status)..."

    # What stopped the run becomes an operation that FAILED, for the history
    # row's errors: a signal by name (it sends no report: its unit fails,
    # which shows it — round 3, M2), an abort (3) with what and why, which
    # the ABORTED report says too (bd DAS-Backup-Manager-2my).
    if [[ -n "$STOP_SIGNAL" ]]; then
        record_op "stopped" "FAIL" "by SIG$STOP_SIGNAL (exit $status)"
    elif [[ "$status" == 3 ]]; then
        note_abort "$rc" "$where"
    fi

    # Record the failed backup run if we were in a real backup and haven't
    # recorded yet — an abort before btrbk too (2my); a dry run records
    # nothing. None of this can cut the trap short and skip the exit below:
    # errexit is off in here (set +e above), and each step soft-fails as well.
    if [[ "$BACKUP_MODE_REAL" == "true" && "$BACKUP_RUN_RECORDED" == "false" ]]; then
        # btrbk's duration so far, if it started; 0 if it never did.
        if (( BTRBK_START_TIME != 0 && BTRBK_END_TIME == 0 )); then
            BTRBK_END_TIME=$(date +%s)
        fi
        record_backup_run_in_db "FAILURE" "$BACKUP_FORCE_FULL"
    fi

    # And say so: one ABORTED report, unless main() sent its report already.
    # Before the unmount, which can hang on a drive that went away. A stop by
    # a signal is not a 3 and sends none: the unit then ends failed, and that
    # shows it.
    if [[ "$status" == 3 && "$BACKUP_MODE_REAL" == "true" && "${REPORT_SENT:-false}" != "true" ]]; then
        send_abort_report "$status"
    fi

    unmount_all
    clear_maintenance_holder

    # Explicit exit, not fallthrough: the run's status ($status) is the
    # process's final exit code, never whatever unmount_all's last internal
    # command happened to return.
    exit "$status"
}

# Installed at top level (not inside main()) so it is active before main()
# is even called — covering argument-parsing aborts (main()'s `exit 1` on an
# unrecognized flag) in addition to every abort at any call depth once
# main() is running. Placed here, after cleanup()'s definition and after the
# BACKUP_MODE_REAL/BACKUP_RUN_RECORDED/BTRBK_END_TIME/SCRIPT_COMPLETED
# globals above are already initialized, so cleanup() can never observe an
# unset variable if something aborts before main() starts. Replaces the old
# `trap cleanup ERR` (installed inside main(), and only ever effective for a
# failing command directly in main()'s own body — see the v4.4.1 header
# note). The signal traps below turn each signal into an `exit` builtin call
# with the conventional 128+signum code, so it routes through this same EXIT
# trap with that status. Bash's default for an untrapped fatal signal is
# worse than it looks: it runs the EXIT trap with whatever $? happened to be,
# then dies by the signal (measured; round 3, M2). A bare
# `trap 'exit' INT TERM` was tried first and rejected: `exit` with no
# argument reuses whatever $? happened to be from the last command that ran
# BEFORE the signal arrived (often 0, e.g. right after a successful
# mountpoint/df call mid-run) — proven empirically in the harness (kill
# -TERM mid-run right after a `true`) to yield exit code 0, which would
# silently satisfy point 5's Sentinel nonzero-exit requirement as a false
# "success" for an operator-initiated `systemctl stop das-backup` abort. The
# explicit-code form was proven in the same harness to yield 143 for TERM /
# 130 for INT regardless of the preceding command's status. See bd
# DAS-Backup-Manager-oeo.
#
# Every signal that ends a run is trapped the same way, and records which it
# was (bd DAS-Backup-Manager-d1r, round 3: M2). HUP (a terminal hanging up),
# PIPE (the stream stdout writes to went away), USR1 and ALRM used to be
# untrapped: bash ran this EXIT trap with $? 0 or 1, cleanup() made it 3 — "a
# command that failed", an ABORTED mail saying the units count it as success
# — and then bash died by the signal anyway, the unit failed. Now each keeps
# its own code, 128 + its number, and STOP_SIGNAL is what tells a stop from a
# command whose status merely looks like one: a pipeline's SIGPIPE under
# pipefail exits 141, a child killed by TERM 143, and neither is a stop
# (measured: in 800 runs with the whole group signalled, bash ran the trap
# before set -e saw the child's status every time). systemd starts units with
# SIGPIPE ignored (IgnoreSIGPIPE=yes), and a shell cannot trap a signal it
# inherited ignored, so under the units PIPE never arrives at all.
STOP_SIGNAL=""
# Called only by the traps below (SC2329: see clear_maintenance_holder).
# shellcheck disable=SC2329
stop_on_signal() { # stop_on_signal <name> <exit code>
    STOP_SIGNAL="$1"
    exit "$2"
}
trap cleanup EXIT
trap 'stop_on_signal HUP 129' HUP
trap 'stop_on_signal INT 130' INT
trap 'stop_on_signal USR1 138' USR1
trap 'stop_on_signal PIPE 141' PIPE
trap 'stop_on_signal ALRM 142' ALRM
trap 'stop_on_signal TERM 143' TERM

# ============================================================================
# MAIN
# ============================================================================

main() {
    local mode="run"
    local force_full="false"

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --dryrun|-n)
                mode="dryrun"
                ;;
            --full|-f)
                force_full="true"
                ;;
            *)
                echo "Usage: $0 [--dryrun|-n] [--full|-f]"
                echo "  --dryrun  Preview the backup: changes no config and sends nothing;"
                echo "            creates only missing empty target directories (e.g. for a"
                echo "            pending adoption), as the real run would"
                echo "  --full    Force recreation of boot subvolumes"
                exit 1
                ;;
        esac
        shift
    done

    # Known from here on, for cleanup(): a real run that aborts once it holds
    # the maintenance lock is recorded and reported, however early; a dry run
    # is neither (bd DAS-Backup-Manager-2my).
    BACKUP_FORCE_FULL="$force_full"
    if [[ "$mode" != "dryrun" ]]; then
        BACKUP_MODE_REAL="true"
    fi

    # Ensure log directory exists
    mkdir -p "$(dirname "$LOG_FILE")"

    echo "========================================"
    echo "  DAS Backup (config-driven)"
    echo "  Mode: $mode"
    echo "  Date: $(date '+%Y-%m-%d %H:%M:%S')"
    echo "========================================"
    echo ""

    log_info "=== DAS Backup Started ==="

    check_root
    # Backup side of the mutual-hold interlock with the scrub engine — must
    # run before any mount work (create_mount_points is the first mounter
    # below). Blocks (deferral, never cancellation) if a scrub currently
    # holds /run/das-maintenance.lock.
    acquire_maintenance_lock

    # Arm cleanup()'s recovery body: acquire_maintenance_lock() just
    # returned, which means this process now holds /run/das-maintenance.lock
    # and is the exclusive owner of the shared DAS mountpoints (the same
    # ones indexer/src/scrub.rs mounts under that lock's protection). Only
    # from this point on is it safe for an abort's cleanup() to call
    # unmount_all() over ALL_TARGET_MOUNTS — see CLEANUP_ARMED's definition
    # in the globals block for the full disaster scenario this closes.
    # Deliberately placed AFTER acquire_maintenance_lock, not after the
    # earlier check_root — check_root() itself can still abort with the
    # recovery body correctly skipped (CLEANUP_ARMED is still "false" then).
    # bd DAS-Backup-Manager-oeo.
    CLEANUP_ARMED="true"

    check_das_connected
    set_io_scheduler
    create_mount_points
    mount_sources
    # Source-side twin of verify_targets_before_btrbk (called further down,
    # after mount_targets). Placement is deliberate and is NOT interchangeable
    # with sitting beside the target guard:
    #
    #   - AFTER mount_sources, because that is the last thing in this script
    #     that mounts a source; before it, a legitimately-unmounted source is
    #     the normal state and refusing would abort every run.
    #   - BEFORE create_snapshot_dirs, because create_snapshot_dirs() is the
    #     FIRST writer to a source path, and is the exact function that left an
    #     empty .btrbk-snapshots directory on the NVMe root when /.btrfs-hdd
    #     was bare. Verifying next to the target guard instead would leave that
    #     writer — the one with a proven artifact on disk — unguarded.
    #
    # Unconditional, dryrun included, matching verify_targets_before_btrbk:
    # create_snapshot_dirs() runs in dryrun too, so the write exists there.
    # An abort here exits 3: the run began and stopped on a source volume's
    # state (EXIT STATUS).
    verify_sources_before_write
    # Between the source guard and the first source writer: sync needs every
    # source mounted and verified, and it reloads the config, so a source it
    # adds gets its snapshot and target directories created below.
    sync_subvolumes "$mode"
    create_snapshot_dirs
    mount_targets
    # Directories only after verification: never under a path not proven to
    # be the expected DAS filesystem (verification reads mount state only and
    # needs none of them).
    verify_targets_before_btrbk
    create_target_dirs

    if [[ "$mode" != "dryrun" ]]; then
        capture_usage "before"
    fi

    run_btrbk "$mode"

    # Still before unmount_all: expiry deletes snapshots on the mounted targets.
    expire_retired_subvolumes "$mode"
    # Still before unmount_all: reads each recovery drive's OS. After btrbk so
    # it can never delay or affect the backup itself.
    check_recovery_os "$mode"

    if [[ "$mode" != "dryrun" ]]; then
        capture_usage "after"
        log_throughput
        update_boot_subvolumes "$force_full"
        show_stats
        run_indexer

        # Record growth data — must still run before unmount (writes to
        # GROWTH_LOG using live per-target usage figures).
        record_growth

        # Prune expired boot archives (daily and full runs alike) while
        # targets are still mounted, before the report is built so pruner
        # failures are visible in overall_status and the email report.
        run_archive_cleanup "$mode"

        # Cache capacity/growth/btrbk-latest report data while targets are
        # still mounted, THEN unmount, THEN build the report. This ordering
        # (vs. the previous report-before-unmount sequence) is what lets an
        # unmount failure appear in the SAME run's email and DB row instead
        # of only the post-unmount console/journal message below. See
        # capture_report_data() and generate_capacity_section()/
        # generate_growth_section() for the cache the report now reads from.
        # Tracks bd DAS-Backup-Manager-ecg.
        capture_report_data
        # The counters are read: decide them — and their FAIL, if they are
        # unknown — before the run status and the report are decided
        # (bd DAS-Backup-Manager-bzw).
        decide_run_counts
        unmount_all

        # Only FAIL makes the run a failure; a WARN (a recovery OS that is
        # stale, or whose boot may run btrbk) shows in the status line and
        # the subject, not in backup_runs.
        local overall_status
        overall_status="$(run_status)"

        local report
        report=$(generate_report)
        echo ""
        echo "$report"
        # A delivery failure used to be logged and nothing else -- no
        # record_op, so the DB row and the (undelivered) report both said the
        # run was fine. Combined with the exit code below, an email outage was
        # invisible to systemd, Sentinel, the GUI and the operator alike.
        # The old message also asserted the backup "completed successfully"
        # regardless of whether it had. bd nsp (b15).
        if ! send_report "$report" "$(subject_status)"; then
            # With email off, send_report fails only when the report could
            # not be saved either, and records that itself (round 4, N4).
            if [[ "${DAS_EMAIL_ENABLED:-false}" == "true" ]]; then
                log_warn "Email delivery failed (run status was: $overall_status)"
                record_op "email" "FAIL" "delivery failed; $(report_whereabouts)"
            fi
            overall_status="FAILURE"
        fi
        # From here an abort has a report already: cleanup() sends no
        # ABORTED one (bd DAS-Backup-Manager-2my).
        REPORT_SENT="true"

        # Record backup run in the database for GUI history
        record_backup_run_in_db "$overall_status" "$force_full"
        report_unrecorded_run
    else
        # Dryrun mode never mutates boot subvolumes or sends an email report,
        # but the pruner still needs a preview pass (its own --dryrun) while
        # targets are mounted — boot-archive-cleanup.sh silently skips any
        # target that isn't currently mounted.
        run_archive_cleanup "$mode"
        unmount_all
    fi

    log_info "=== DAS Backup Completed ==="
    echo ""
    # Safe only on the unmount gate's own OK: a FAIL — a target still
    # mounted, or one the probe could not tell about — or no answer at all is
    # NOT safe, and the line says why (bd DAS-Backup-Manager-jug6).
    if [[ "${OP_STATUS[unmount]:-}" == "OK" ]]; then
        log_info "Backup complete. DAS can be safely disconnected."
    else
        log_warn "Backup complete, but NOT every backup target is known to be unmounted — DAS is NOT safe to disconnect: ${OP_STATUS[unmount_detail]:-the unmount step recorded no result}"
    fi

    # ---- Process exit status: 0 or 3 (EXIT STATUS in the header) ----------
    #
    # It was once whatever `SCRIPT_COMPLETED="true"` returned, i.e. always 0,
    # so btrbk could fail outright at 03:00 and `systemctl status` stayed
    # green (bd nsp c1). Then it followed btrbk alone and was 1 whenever btrbk
    # exited nonzero — and btrbk exits 10 when any ONE target aborts (measured
    # 2026-10-02 by hand, recovery drive A pulled: btrbk 10, this script 1).
    # Under the unit one absent drive is `failed` on every run, and
    # cachyos-sentinel, which restarts a failed unit and whose limiter (3
    # restarts per 600 s) cannot brake a loop slower than ten minutes, would
    # start a whole new ~25-minute backup about every ten minutes until the
    # drive came back (bd DAS-Backup-Manager-d1r).
    #
    # Now 0 when no operation FAILED and 3 when any did — the operations the
    # report and the history row are made from (completed_exit_status). The
    # units list SuccessExitStatus=3, so a failure the next start would meet
    # again never leaves them failed; it travels by the report, the history
    # row and the journal's status=3, as 18p's split does for the scrub.
    local status
    status="$(completed_exit_status)"
    if [[ "$status" != 0 ]]; then
        log_error "This run had failures: $(failed_ops) — exit status $status"
    fi

    # Marks the completion path so the EXIT trap (cleanup()) exits with this
    # status instead of re-running unmount_all/record_backup_run_in_db, which
    # main() has already run itself by this point on every reachable path
    # (real-run and dryrun alike). bd DAS-Backup-Manager-oeo.
    SCRIPT_COMPLETED="true"
    return "$status"
}

main "$@"; exit $?
