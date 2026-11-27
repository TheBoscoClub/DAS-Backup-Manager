# Another Program's Mount of a Recovery Drive — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

*Status, plan and decisions: bd `DAS-Backup-Manager-eg4j.19`. Checkboxes not maintained after the fact.*

**Goal:** every path that meets another program's mount of a recovery drive's partitions clears it by the
operator's stop policy and continues, refusing only where the policy says so.

**Architecture:** one partition-keyed clearing in `indexer/src/mount.rs` (built on `holders.rs`), reached by a new
hidden `btrdasd recovery-os clear-drive` (for `recovery-os-vm.sh`), by `read_recovery_esp` (read-only), and by
the target-mount paths for every target with a `mount_uuid` (Rust and `backup-run.sh`).

**Tech Stack:** Rust 2024 (`buttered_dasd`, `btrdasd`), bash (`scripts/recovery-os-vm.sh`, `scripts/backup-run.sh`).

**Spec:** `docs/superpowers/specs/2026-11-27-recovery-session-clears-host-mounts-design.md`

## Global Constraints

- Stop policy unchanged: writer never signalled; PID 1, kernel threads, the job and its ancestors never signalled;
  readers SIGTERM, `TERM_GRACE` 10 s, SIGKILL, `KILL_WAIT` 5 s; start time re-read before each signal.
- A writing job (session, provision, backup, scrub) stops readers; a read-only job (`stop_readers = false`)
  signals nothing, may unmount an idle mount, refuses on a holder and names it.
- A dry run / `--report-only` signals nothing and unmounts nothing.
- The drive is found only by its configured serial; the ESP only by `esp_partuuid` listed on that drive. No ESP
  discovery by type.
- Every clearing runs under the held maintenance lock (`DAS_MAINTENANCE_LOCK_FD` for a child).
- Tests never signal a real process; `report::MAILX` stays refused in test builds; never edit working code to watch
  a test fail (faults through fixtures only); no `cargo mutants` (CI only).
- Build scratch: `CARGO_TARGET_DIR=/tmp/das-eg4j19-target`; delete it when the job ends.
- Placeholders only in tracked files (`SERIALnn`, `sdX1`, `RECOV-ESP-<bay>`, `<recovery target>`).
- Shell: `shellcheck` as CI runs it (default severity, all three globs), `shfmt`; Rust: `cargo fmt`, `cargo clippy -- -D warnings`.

## Review Focus

1. A vfat ESP mounted at two places (udisks plus a bind): both must come down, deepest first, or the job refuses.
2. A mount of the drive's partition that is stacked under by another filesystem: refused, never unmounted.
3. The partition vanishes between the scan and the unmount (drive unplugged): `CANNOT-RUN`/refused, never "OK".
4. `mounted_partitions` lists a mount that `clear-drive` then finds gone (raced away): OK, continue — the
   re-check is the authority.
5. A recovery target whose `serial` resolves to no device: `CANNOT-RUN`, the session refuses as today.

Each has its test in the owning task.

---

### Task 1: Partition-keyed clearing core

**Files:** Modify `indexer/src/holders.rs` (beside `mounts_of`), `indexer/src/mount.rs` (beside `clear_mounted_elsewhere`).

**Interfaces:**
- Produces: `holders::mounts_of_partitions(entries: &[MountEntry], partitions: &[String]) -> Vec<MountEntry>` —
  any fstype, source matched as given and canonicalized, deepest mount point first (same order as `mounts_of`).
- Produces: `mount::clear_partitions_with(runner: &dyn CommandRunner, site: &ClearSite<'_>, who: &str,
  partitions: &[String], btrfs_fsid: Option<&str>, progress: &dyn ProgressCallback) -> Result<Option<Vec<String>>, String>`
  — `Ok(None)` mounted nowhere; `Ok(Some(sent))` every mount cleared and unmounted (`sent` = signals, empty when none;
  in report-only, the "would …" lines); `Err(why)` refused. Same steps as `clear_mounted_elsewhere`: stacked-under
  refusal, `maintenance_on` when `btrfs_fsid` is `Some`, `clear_logged`, `UMOUNT_ATTEMPTS` with re-clear, re-read
  mountinfo; the `/sys/fs/btrfs/<fsid>` hidden-mount check when `btrfs_fsid` is `Some`.
- `clear_mounted_elsewhere` keeps its behaviour; sharing the unmount loop between the two is the builder's call.

- [ ] Tests first (in `holders.rs` / `mount.rs` test modules, using the existing scripted host + `ClearSite` fixture):
  `mounts_of_partitions_matches_vfat_and_btrfs_by_partition`, `mounts_of_partitions_ignores_other_partitions`,
  `clear_partitions_idle_vfat_mount_is_unmounted` (Ok(Some(empty)), one `umount` of the mount point),
  `clear_partitions_reader_is_stopped_when_job_writes`, `clear_partitions_reader_refuses_read_only_job` (Err names pid),
  `clear_partitions_writer_refuses_and_signals_nothing`, `clear_partitions_two_mounts_come_down_deepest_first`
  (Review 1), `clear_partitions_stacked_mount_refuses` (Review 2), `clear_partitions_report_only_unmounts_nothing`.
- [ ] Run `cd indexer && CARGO_TARGET_DIR=/tmp/das-eg4j19-target ~/.claude/bin/bounded-test -- cargo test --features dbus -- clear_partitions mounts_of_partitions` → the new tests fail (missing functions).
- [ ] Implement; run again → pass. Commit `feat(mount): partition-keyed clearing for any filesystem (bd DAS-Backup-Manager-eg4j.19)`.

### Task 2: `btrdasd recovery-os clear-drive`

**Files:** `indexer/src/mount.rs`, `indexer/src/main.rs` (`RecoveryOsAction`, beside `HoldDisk`; dispatcher in `run_recovery_os`).

**Interfaces:**
- Consumes: Task 1.
- Produces: `pub enum ClearDrive { Cleared(String), Refused(String), CannotRun(String) }` with
  `line(&self) -> (String, i32)` → `CLEAR DRIVE: OK  (<d>)` 0 / `CLEAR DRIVE: REFUSED  (<d>)` 3 /
  `CLEAR DRIVE: CANNOT-RUN  (<d>)` 2 (mirror `ClearTarget::line`).
- Produces: `pub fn clear_drive(target: &Target, report_only: bool, progress: &dyn ProgressCallback, held: &MaintenanceHeld) -> ClearDrive`
  and a testable `clear_drive_with(runner, probes, site, target, progress) -> ClearDrive`: drive by
  `(probes.device_from_serial)(&target.serial)` (none → CannotRun), partitions by
  `lsblk -nrp -o NAME,TYPE <disk>` rows of TYPE `part` (lsblk fails → CannotRun), `btrfs_fsid = target.mount_uuid`,
  `stop_readers = true`. A target with more than one serial → CannotRun ("not a single-drive target").
- CLI: `RecoveryOsAction::ClearDrive { label: String, report_only: bool, config: PathBuf }`, `#[command(hide = true)]`,
  doc text in the style of `ClearTarget`'s; requires the handed-down lock exactly as `cmd_clear_target` (same
  `CANNOT-RUN … DAS_MAINTENANCE_LOCK_FD` refusal). Prints each progress line, then the `CLEAR DRIVE` line; exits its code.
- Each mount's line before the result: `<partition>  <label|->  <fstype> <rw|ro>  <mount point>  mounted by: <Origin>`
  then `holders: none -> unmounted` / `holder: …` / signal lines (the `holders::Holder` Display).

- [ ] Tests first: `clear_drive_unknown_serial_cannot_run` (Review 5), `clear_drive_two_serials_cannot_run`,
  `clear_drive_lsblk_failure_cannot_run`, `clear_drive_nothing_mounted_is_ok`, `clear_drive_idle_esp_unmounted_ok`,
  `clear_drive_writer_refused_exit_3`, `clear_drive_partition_gone_before_unmount_is_not_ok` (Review 3: umount fails
  every attempt → Refused), `clear_drive_parses_cli` (clap), `clear_drive_without_lock_fd_cannot_run_exit_2`.
- [ ] Run → fail; implement; run → pass. Commit `feat(cli): recovery-os clear-drive (bd DAS-Backup-Manager-eg4j.19)`.

### Task 3: `recovery-os-vm.sh` session and provision use it

**Files:** `scripts/recovery-os-vm.sh` (`check_not_mounted`, its two call sites in `cmd_session` and
`provision_preflight`, `write_history`), `tests/test_recovery_os_vm.sh`, the script's version header.

**Interfaces:** Consumes Task 2's CLI and exit codes.

- `check_not_mounted`: `mounted_partitions` empty → return. Else print `[$LABEL] DRIVE MOUNTED ON HOST`, run
  `DAS_MAINTENANCE_LOCK_FD=$LOCK_FD "$BTRDASD" recovery-os clear-drive --label "$LABEL" [--report-only under --dry-run]`
  capturing output with the status line (`fail-silent.md` rule 7 form), print its lines indented by 4, then:
  0 and `mounted_partitions` now empty → `log "drive clear -- continuing"`; 0 under `--dry-run` → refuse
  "dry run: … nothing was stopped or unmounted" (never continue beside a mount); 3 → refuse
  `drive in use on this host -- <detail>`; 2 or other → refuse `cannot clear the drive's host mounts: <detail>`;
  0 but still mounted (Review 4's reverse) → refuse `still mounted after clearing: …`.
- **Lock order (read 2026-11-27):** `cmd_session` runs `check_not_mounted` *before* `take_lock` and before
  `HISTORY_ARMED=true`; `cmd_provision` runs `provision_preflight` (which holds the call) before its `take_lock`, and its
  `--dry-run` exits before taking the lock. So: split `check_not_mounted` into the existing early listing (kept
  where it is, now only logging the mounts it found, never refusing) and a new `clear_host_mounts` run straight
  after `take_lock` (and, in `cmd_session`, after `HISTORY_ARMED=true` for a real run, so a refusal reaches the
  history with its `host_mounts`). A drive session of a two-drive run inherits the lock through
  `DAS_RECOVERY_VM_LOCK_FD`; its `take_lock` adopts it first, so the same placement holds. `refuse` exits 1 and the
  existing exit path releases the lock — confirm by reading, do not change it.
- **Dry runs:** a session dry run takes the lock → `clear-drive --report-only`, then refuse as above. A provision
  dry run holds no lock → no `clear-drive`; it prints the mount listing and `a real run clears these by the stop
  policy (writers refuse)`, and its dry-run result stands.
- `write_history`: a `host_mounts` field — the list of cleared/refused mount lines (array of strings), `[]` when none.
- **What the GUI shows:** the helper runs sessions through `run_script_merged` (stdout and stderr on one pipe, order
  kept). Every line of the block goes out with `$LOG_PREFIX` through `log`/`warn`, so it lands whole and in order;
  a helper test asserts the block's lines arrive in order in the relayed text (`indexer/src/bin/btrdasd-helper.rs` tests).

- [ ] VM-suite cases first (stub `btrdasd` on the suite's PATH as the suite already does; never a self-calling stub):
  `session_idle_host_mount_is_cleared_and_session_continues`, `session_writer_host_mount_refuses_with_block`,
  `session_clear_drive_cannot_run_refuses`, `session_mount_gone_before_clear_continues` (Review 4),
  `session_dry_run_with_host_mount_unmounts_nothing`, `provision_preflight_clears_host_mount`,
  `history_line_carries_host_mounts`, `refused_session_history_carries_host_mounts`,
  `pair_run_drive_session_clears_under_inherited_lock`, `provision_dry_run_lists_mounts_without_clearing`.
- [ ] Run the suite alone (`systemd-run --user --scope --quiet -p TasksMax=600 bash tests/test_recovery_os_vm.sh`, ~5 min, output to a file) → new cases fail; implement; run → pass.
  Commit `feat(recovery-os-vm): a host mount of the drive is cleared by the stop policy, not just refused (bd DAS-Backup-Manager-eg4j.19)`.

### Task 4: The ESP reading clears an idle mount, read-only

**Files:** `indexer/src/mount.rs` (`mount_read_unmount`), `indexer/src/recovery_os.rs` (the ESP row's level), `scripts/backup-run.sh` if it sets the row's level.

**Interfaces:** Consumes Task 1 with a `ClearSite` whose `stop_readers = false`; `read_recovery_esp`'s signature
unchanged (production builds `host_clear_site(false, false)` inside; the `_with` variant takes the site for tests).

- Mounted at a place other than its own directory → `clear_partitions_with(…, &[<ESP partition path by PARTUUID>], None, …)`:
  `Ok` → continue to mount at its own directory and read; `Err(why)` → `Err("{spec} is mounted at {there}, {why} — not read")`.
- That `Err` (and every "not checked" that is not "no `esp_partuuid` is configured") is a WARN row in the reading and
  the backup report, not INFO.

- [ ] Tests first: `esp_mounted_elsewhere_idle_is_unmounted_and_read`, `esp_mounted_elsewhere_held_by_reader_not_read_nothing_signalled`,
  `esp_not_checked_because_held_is_warn`, `esp_not_configured_stays_info`.
- [ ] Run → fail; implement; run → pass. Commit `feat(mount): the ESP reading takes down another program's idle mount of it (bd DAS-Backup-Manager-eg4j.19)`.

### Task 5: Single-drive targets get the clearing; no backup writes through another program's mount

**Files:** `indexer/src/mount.rs` (`ensure_targets_mounted_with`: the `guarded` gate on clearing, the
"Already mounted elsewhere — bind-mount" fallback), `scripts/backup-run.sh` (the `is_guarded_target` branch at the
target-mount loop; `clear_target`), `indexer/src/main.rs` (`ClearTarget` doc text), `backup-run.sh` version header.

**Interfaces:** `clear_target_with` / `clear_mounted_elsewhere` accept any target with a non-empty `mount_uuid`
(members by filesystem UUID bound to configured serials; confirm `raid_guard::MemberHost::members` serves one
serial, else a single-device lookup). RAID-1-only parts (guarded mount, missing-devid checks) stay gated on `is_guarded`.

- Rust: a target with `mount_uuid` mounted elsewhere → clearing (writing jobs stop readers; read-only jobs refuse on
  a holder, unmount an idle one), then its normal mount; the bind fallback stays only for a target with no `mount_uuid`.
- Script: `clear_target` runs for every target with a uuid before mounting; the dry-run and exit-3 handling (WARN /
  FAIL row, NOT available, `rmdir`) as the RAID-1 branch does today; then the existing single-device mount.
- Scrub (`indexer/src/scrub.rs` `ensure_mounted`, sweep by mount site 2026-11-27): its non-guarded branch mounts
  `UUID=` beside another program's mount; it clears first as a writing job (scrub repairs), through the same
  Task 1 / `clear_mounted_elsewhere` code; refused → that filesystem is not scrubbed, named, as a guarded refusal is.
- Delegated hold (`held.is_delegated()` branches in `ensure_targets_mounted_with` and `scrub::ensure_mounted`):
  another program's mount of a single-drive target is not cleared under a handed-down hold and never bound — NOT
  available, as the RAID-1 branch does today (the parent owns the mounts).
- Read-only users of `ensure_targets_mounted` (doctor, walk, restore, `recovery-os record`) get the read-only
  behaviour for single-drive targets: idle mount unmounted, holder → that target refused, named.

- [ ] Tests first: Rust `single_drive_target_mounted_elsewhere_is_cleared_then_mounted`,
  `single_drive_target_writer_elsewhere_not_available`, `single_drive_target_never_bind_mounted_when_uuid_configured`,
  `target_without_uuid_still_bind_mounts`, `scrub_single_drive_mounted_elsewhere_cleared_then_scrubbed`,
  `scrub_single_drive_writer_elsewhere_not_scrubbed`, `delegated_hold_single_drive_elsewhere_not_available`,
  `doctor_single_drive_reader_elsewhere_refused_nothing_signalled`; shell (the backup-run suite that covers `clear_target`):
  `mirror_target_mounted_elsewhere_cleared_then_mounted`, `mirror_target_clear_refused_fail_row_not_available`.
- [ ] Run → fail; implement; run → pass. Commit `feat(backup): a single-drive target mounted by another program is cleared by the stop policy, never written through (bd DAS-Backup-Manager-eg4j.19)`.

### Task 6: Docs, rules, CHANGELOG, full suite

**Files:** `.claude/rules/backup.md` (the stop-policy bullets: "any target with a `mount_uuid`"; recovery-OS
session clears host mounts), `.claude/rules/esp-safety.md` (§The one ESP mount this project makes: it also takes
down another program's idle mount of a target's own ESP, found only by `esp_partuuid`), `.claude/docs/rules-reference/backup.md`,
`docs/` guides naming the old refusal (`grep -rn "unmount it first\|left as it is" docs .claude`), `CHANGELOG.md` (Unreleased).

- [ ] Update every hit; `markdownlint`, `codespell` on changed docs.
- [ ] Full suites once: `~/.claude/bin/bounded-test -- cargo test --features dbus` (all), `ctest --test-dir build`, VM suite alone. Every result line read.
- [ ] Commit `docs: another program's mount of a recovery drive (bd DAS-Backup-Manager-eg4j.19)`.

### Task 7: Install and reproduce for the operator's GUI test (native, live host)

- [ ] Busy check from `backup.md` (no das-backup/das-scrub unit active, the singleton free); `cmake --build build`, `cmake --install build`, `sudo btrdasd setup --upgrade`; `cmp` the installed binaries and script against the build.
- [ ] Reproduce, with the operator's OK first (a hand mount of a recovery ESP): `udisksctl mount -b /dev/disk/by-partuuid/<B's esp_partuuid> -o ro` as root, the way fwupd's mount came through udisks (a `systemctl restart fwupd` is not expected to leave one: yesterday's leftover came from a transient busy unmount). `findmnt -S PARTUUID=<…>` shows it.
- [ ] Tell the operator it is installed and the condition is in place; his GUI run is the acceptance test. Record the result in bd `eg4j.19`.
