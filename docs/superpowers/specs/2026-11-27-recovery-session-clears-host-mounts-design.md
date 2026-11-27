# A recovery-OS session clears what the host left mounted on its drive — design

Short design, approved by the operator 2026-11-27. **Status, plan and decisions: bd `DAS-Backup-Manager-eg4j.19`.**
Cause of the triggering mount: bd `DAS-Backup-Manager-eg4j.18`.

## Intent

A recovery-OS session (GUI, helper, timer or command line) that finds a partition of its drive mounted on the host
today refuses: `REFUSED: /dev/sdX is mounted on this host: sdX1 on /run/media/root/RECOV-ESP-<bay> -- unmount it first`.
On 2026-11-27 that mount was a leftover: fwupd mounted the ESP at startup, failed to unmount it ("target is busy"),
and nothing used it afterwards. The refusal pointed at the problem and then stopped.

The operator's stop policy (2026-11-23, bd `su1b.12`): find what has it mounted and, if possible, why; whether a
process uses it for reading or writing; "if whatever has it mounted can be killed off or the array unmounted, then
do so and continue on". 2026-11-25: only a job that writes under a target stops readers. That policy was built for
backup targets only; the recovery-OS session never got it. This applies it there.

## Goals

- The session identifies every mount of every partition of its drive (the ESP and the backup filesystem alike):
  device, label, filesystem, rw/ro, mount point, origin (udisks, a systemd `.mount` unit, a hand mount).
- It reads every process using each mount and its mode (writer, reader), with pid, cmdline and unit.
- No holder → unmount, read the mount table again, continue the session.
- Only readers → the stop policy (SIGTERM, 10 s, SIGKILL, 5 s; start time re-read before each signal), each signal
  named, then unmount and continue. The session writes the whole drive, so it is a writing job.
- A writer, PID 1, a kernel thread, the session or its ancestors, a mount that will not come down, or a mount table
  that cannot be read → refuse; the two-drive run then still does not start the other drive.
- Every outcome: a short block easy to read at a glance in the log, the GUI and the journal, and the same facts as
  a field in `recovery-os-vm-history.jsonl`.

## Non-goals

- Fixing fwupd's ESP discovery (host side, CachyOS-Kernel; bd `eg4j.18`).
- Changing the stop policy itself, or the backup path's use of it.

## Anti-goals

- A second stop policy in bash that can drift from `holders.rs`.
- Signalling a writer, or anything outside the stop policy's set, for any reason.
- A dry run that signals or unmounts: `--dry-run` reports "would stop" / "would unmount" and changes nothing.
- Touching any partition not on the configured drive, or finding the drive by anything but its configured serial.

## Constraints

- Runs under the maintenance lock the session already holds; handed to `btrdasd` by `DAS_MAINTENANCE_LOCK_FD`.
- The ESP rules (`esp-safety.md`, global `esp-ownership.md`): this acts only on the session's own configured drive,
  named by serial; no discovery of ESPs by type.
- Tests never signal a real process (`holders::SystemSignaller` refuses in test builds); the VM suite runs alone.

## Approaches

1. **Reuse `holders.rs` behind a new `btrdasd` subcommand** (chosen): one policy, already reviewed and
   installed; the script only reads its result line.
2. Extend `backup clear-target` with a whole-drive mode: same code, but it is keyed by a target's filesystem and
   its RAID members; a drive with an ESP is a different object, and the command's name and exit contract would
   blur.
3. Reimplement in `recovery-os-vm.sh` with `fuser`/`lsof`: rejected — a second policy (anti-goal 1).

## Design

- `btrdasd recovery-os clear-drive <label> [--report-only]`: resolves the target's drive by its configured
  serial (`/dev/disk/by-id/…`), collects the mounts of every partition on it from mountinfo, and runs
  `holders::clear_holders` with `stop_readers = true`; then unmounts each and reads mountinfo again.
  Prints one line per mount, holder and signal, then `CLEAR DRIVE: OK|REFUSED|CANNOT-RUN  (<detail>)`;
  exit 0 / 3 / 2, as `clear-target`.
- `check_not_mounted` (both call sites) calls it when `mounted_partitions` lists anything, after the
  maintenance lock is held; OK → re-check `mounted_partitions` empty and continue; otherwise refuse with the block.
- What the operator sees:

  ```text
  [<recovery target>] DRIVE MOUNTED ON HOST
    sdX1  RECOV-ESP-<bay>  vfat rw  /run/media/root/RECOV-ESP-<bay>  mounted by: udisks (root)
          holders: none -> unmounted
  [<recovery target>] drive clear -- continuing
  ```

  A refusal shows the holder, e.g. `pid 4121 kate (writing EFI/loader/loader.conf) -- writer, not signalled`,
  then `REFUSED: drive in use on this host -- …`.
- History: a `host_mounts` field in the session's line (each mount, origin, holders, action, outcome).

## Left to the builder

- The exact line and field layout, matching `clear-target`'s and the history file's existing style.
- Where the call sits relative to `take_lock` on each path (it must hold the lock).

## Proof

- Unit tests over a built `/proc` and mountinfo (as `holders.rs` does); a VM-suite case with a fixture mount of a
  loop disk's partition: no holder → unmounted and continued; reader → stopped; writer → refused.
- Live: the operator's GUI run, after reproducing the real condition (expected, not yet observed: `systemctl restart fwupd` remounts the ESP as at startup; if not, a read-only udisks mount by hand).
