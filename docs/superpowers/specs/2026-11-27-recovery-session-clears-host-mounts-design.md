# Another program's mount of a recovery drive is cleared by the stop policy, on every path — design

Short design, approved by the operator 2026-11-27; widened the same day to every path the sweep found (operator:
"it's part of the same design"). **Status, plan and decisions: bd `DAS-Backup-Manager-eg4j.19`.**
Cause of the triggering mount: bd `DAS-Backup-Manager-eg4j.18`.

## Intent

A recovery-OS session (GUI, helper, timer or command line) that finds a partition of its drive mounted on the host
today refuses: `REFUSED: /dev/sdX is mounted on this host: sdX1 on /run/media/root/RECOV-ESP-<bay> -- unmount it first`.
On 2026-11-27 that mount was a leftover: fwupd mounted the ESP at startup, failed to unmount it ("target is busy"),
and nothing used it afterwards. The refusal pointed at the problem and then stopped.

The operator's stop policy (2026-11-23, bd `su1b.12`): find what has it mounted and, if possible, why; whether a
process uses it for reading or writing; "if whatever has it mounted can be killed off or the array unmounted, then
do so and continue on". 2026-11-25: only a job that writes under a target stops readers; a read-only job signals
nothing, refuses on a holder and names it. The policy was built for RAID-1 backup targets only. This applies it to
every path that meets another program's mount of a recovery drive's partitions.

## The paths (sweep 2026-11-27)

| # | Path | Today | Job kind |
| --- | --- | --- | --- |
| 1 | Recovery-OS session (`recovery-os-vm.sh` `cmd_session`) | refuses on any mount of the drive | writes (the VM writes the whole disk) |
| 2 | Provision (`provision_preflight`) | the same `check_not_mounted` refusal | writes |
| 3 | Recovery-OS reading of the ESP (`mount::read_recovery_esp`: nightly, `recovery-os record`) | `ESP not checked: … is mounted at … — left as it is`, INFO | read-only |
| 4 | A single-drive (`mirror`) target's filesystem mounted elsewhere: backup (`backup-run.sh`, Rust/GUI), scrub | bind-mounts the other program's mount and writes through it (`mount.rs` "Already mounted elsewhere"; `backup-run.sh` mounts beside it); bd 804 WARN only | backup, scrub write; others read-only as today |
| 5 | `holders::mounts_of` | matches btrfs mounts only: a vfat ESP mount is invisible | — |

## Goals

- Every mount of the partition(s) a job is about is identified: device, label, filesystem, rw/ro, mount point,
  origin (udisks, a systemd `.mount` unit, a hand mount).
- Every process using each is read with its mode (writer, reader), pid, cmdline and unit.
- No holder → unmount, read the mount table again, continue.
- Only readers, and a writing job → the stop policy (SIGTERM, 10 s, SIGKILL, 5 s; start time re-read before each
  signal), each signal named, then unmount and continue.
- A read-only job (3; the read-only jobs of 4) → signals nothing; an idle mount is unmounted and the job continues;
  a holder → the job does not use it and names the holder (3: a WARN row, not INFO).
- A writer, PID 1, a kernel thread, the job or its ancestors, a mount that will not come down, or a table that
  cannot be read → refuse: 1 and 2 refuse the session (the two-drive run still does not start the other drive);
  4 leaves the target NOT available this run with a FAIL row, as a RAID-1 target is today.
- Every outcome: a short block easy to read at a glance in the log, the GUI and the journal; for 1 and 2 the same
  facts as a `host_mounts` field in `recovery-os-vm-history.jsonl`.

## Non-goals

- Fixing fwupd's ESP discovery (host side, CachyOS-Kernel; bd `eg4j.18`).
- Changing the stop policy itself, or the RAID-1 guard's mount.

## Anti-goals

- A second stop policy in bash that can drift from `holders.rs`.
- Signalling a writer, or anything outside the stop policy's set, for any reason.
- A dry run that signals or unmounts: it reports "would stop" / "would unmount" and changes nothing.
- Touching any partition not on the configured drive, or finding the drive by anything but its configured serial;
  no discovery of ESPs by type (`esp-safety.md`, global `esp-ownership.md`).
- Writing a backup through another program's mount of a target.

## Constraints

- Runs under the maintenance lock the job holds; scripts hand it down by `DAS_MAINTENANCE_LOCK_FD`.
- Tests never signal a real process (`holders::SystemSignaller` refuses in test builds); the VM suite runs alone.

## Approaches

1. **One clearing in `holders.rs`/`mount.rs`, reached by every path** (chosen): the reviewed, installed policy;
   scripts read one result line.
2. Per-path code (bash in `recovery-os-vm.sh`, a branch in the backup path): rejected, anti-goal 1.
3. Leave 3 and 4 as warnings: rejected by the operator 2026-11-27.

## Design

- **5:** a partition-keyed lookup beside `mounts_of` (any filesystem type, a partition's path as given and
  canonicalized); `mounts_of` itself unchanged for the RAID guard.
- **1, 2:** `btrdasd recovery-os clear-drive --label <label> [--report-only]` (hidden, like `clear-target`):
  resolves the target's drive by its configured serial (`/dev/disk/by-id/…`), takes every partition on it, and
  clears every mount of them as a writing job; then unmounts each and reads mountinfo again. One line per mount,
  holder and signal, then `CLEAR DRIVE: OK|REFUSED|CANNOT-RUN  (<detail>)`; exit 0 / 3 / 2. `check_not_mounted`
  (both call sites) calls it when `mounted_partitions` lists anything, under the held lock; OK → re-check empty and
  continue; else refuse with the block.
- **3:** `read_recovery_esp` finds the ESP (by its `esp_partuuid`, as now) mounted elsewhere → the read-only
  clearing: idle → unmount, then read as normal; held → not read, `ESP  not checked: held by <holder>`, a WARN.
- **4:** the su1b.12 clearing of "mounted elsewhere" runs for any target with a `mount_uuid`, not only RAID-1
  (`backup-run.sh` `clear_target` before mounting, Rust `ensure_targets_mounted`); the bind-to-another-program's
  mount fallback goes for such targets. The RAID-1 guarded mount stays only for RAID-1 targets.
- What the operator sees (1, 2):

  ```text
  [<recovery target>] DRIVE MOUNTED ON HOST
    sdX1  RECOV-ESP-<bay>  vfat rw  /run/media/root/RECOV-ESP-<bay>  mounted by: udisks (root)
          holders: none -> unmounted
  [<recovery target>] drive clear -- continuing
  ```

  A refusal shows the holder, e.g. `pid 4121 kate (writing EFI/loader/loader.conf) -- writer, not signalled`,
  then `REFUSED: drive in use on this host -- …`.

## Left to the builder

- The exact line and field layout, matching `clear-target`'s and the history file's existing style.
- Where the call sits relative to `take_lock` on each path (it must hold the lock).
- Whether `clear_mounted_elsewhere` needs a single-device member lookup or the RAID one serves (it finds members
  by filesystem UUID and binds them to configured serials).

## Proof

- Unit tests over a built `/proc` and mountinfo (as `holders.rs` does) for each path's decision.
- VM suite: a fixture mount of a loop disk's partition: no holder → unmounted and continued; reader → stopped;
  writer → refused.
- Live: the operator's GUI run, after reproducing the real condition (expected, not yet observed: `systemctl
  restart fwupd` remounts the ESP as at startup; if not, a read-only udisks mount by hand).
