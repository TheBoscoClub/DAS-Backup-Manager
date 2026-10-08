#!/bin/bash
# recovery-os-vm.sh - update a recovery drive's own OS by booting it in a VM
# Version: 2.1.0
# Date: 2026-10-08
#
# Each role = "mirror" target (a 2 TB recovery drive) carries a fully
# independent install: its own ESP on partition 1, its own root as subvolume
# @ on partition 2 -- and that partition-2 filesystem also receives backups
# from the host. This boots that OS in a libvirt domain of its own --
# recovery-os-updater-<label>, one per role = "mirror" target, each with its
# own NVRAM, console, log and guard channel, all rendered from one template
# (define) -- with the WHOLE physical disk passed through, so it can be
# updated from inside -- keyring, full upgrade, reboot, verify -- without
# rebooting the workstation. bd DAS-Backup-Manager-7wb; the unattended
# update, both drives in one run, the egress rule, the session history and
# the console bridge: bd DAS-Backup-Manager-8249.
#
# What a session guarantees, and how:
#   - The host never mounts or writes the drive while the VM has it. Two
#     kernels mounting one BTRFS filesystem corrupt it. Two guards:
#       1. /run/das-maintenance.lock, taken without waiting and held for the
#          whole session: backups and scrubs wait for it, and the other jobs
#          that mount targets defer.
#       2. `btrdasd recovery-os hold-disk` holds the whole disk open O_EXCL.
#          While it does, the kernel refuses to mount any partition of it, by
#          device or by UUID, in any mount namespace -- while qemu's own
#          non-exclusive open still works (proven on a test VM, bd
#          DAS-Backup-Manager-frb). The holder runs in a session of its own
#          (setsid: no Ctrl-C or hangup reaches it) and in a scope of its own
#          (systemd-run --scope, outside the operator's login session, so
#          ending that session or its user manager does not end it), and it
#          inherits the lock's descriptor: the claim AND the lock outlive this
#          script if it is killed while the VM still runs. The lock's record
#          names the holder, which is what holds it. A holder that dies while
#          the VM runs is replaced at the next check, and tried for at every
#          check until a claim holds again; a session that lost its claim at
#          all ends with exit 5, saying whether it was taken again.
#   - Only a role = "mirror" target: found by the serial config.toml gives
#     it, the serial read back from the disk, and partition 2 carrying the
#     filesystem config mounts that target by (mount_uuid). A serial that also
#     belongs to a role = "primary" target is refused, whatever the mirror
#     entry says.
#   - SATA, not virtio: the recovery OS's initramfs almost certainly lacks
#     virtio drivers. Its default image boots on SATA only if it was built
#     with ahci (installed on a machine with an AHCI controller); one built
#     with the drive already in the USB enclosure may lack it, and the first
#     boot in the VM then needs the fallback entry -- the documented path,
#     not a defect.
#   - btrbk cannot run in the booted OS (bd 1yg, 0zm): the drive holds the
#     received backups on the same partition 2, and a btrbk run by that OS
#     could delete or rewrite them. Two layers:
#       1. The boot record. The nightly backup run records, per drive, what
#          booting its OS would run (`btrdasd recovery-os status
#          --state-file`, /var/lib/das-backup/recovery-os.json) and one
#          verdict: will, may or no. All three go on attended, the guard
#          enforcing -- "will" with a banner telling the operator to disable
#          it in the session (decision 3 of bd 8249); --unattended refuses
#          "will", whatever the options. The record must also be at most
#          MAX_RECORD_AGE_DAYS old and made after this drive's last session
#          (the OS may have changed in it; the time of each session is kept
#          in recovery-os-vm-sessions beside the record, written just before
#          the OS is resumed and again when the disk is back -- never for a
#          session that ended before the resume: that OS never ran, so its
#          record still describes it and a retry is admitted, bd
#          DAS-Backup-Manager-dmxt).
#          --accept-boot-record-risk lets an age or a session
#          through, loudly and into the summary -- never a record that is
#          missing, of another schema, or says nothing about this drive,
#          and never one that is not of this drive's filesystem: its
#          mount_uuid (bd df0) must be the one config mounts the target by,
#          which partition 2 must carry too, so a label pointed at another
#          drive, or a filesystem made again, never inherits the old verdict.
#          The rule that makes the verdict is btrdasd's; this only reads it
#          (jq).
#          Advisory on its own: no static reading of shell and systemd is
#          complete, and a stock install reads "may" for ever.
#       2. The session guard, the enforcement. For this session only, the
#          domain is defined with SMBIOS Type 11 strings that systemd >= 256
#          in the guest turns into units (systemd.system-credentials(7)):
#          btrbk's own units, every cron daemon and every unit the record
#          names as running btrbk are masked -- an empty unit plus a drop-in,
#          sorting last, whose condition can never hold -- and
#          das-vm-guard.service binds a btrbk that refuses (and says so on
#          the kernel log) over every btrbk in the usual bin directories,
#          before udev's coldplug and sysinit.target. A record naming a unit
#          the guard cannot mask is refused on "will" or "may".
#          das-vm-guard-report.service checks all of it and writes a line to
#          the virtio port org.dasbackup.guard -- at the start of every boot
#          and every minute after, each with the boot's id and a sequence
#          number -- which lands in a root-only file here (followed across
#          virtlogd's rotation). Every line is judged: each boot must begin
#          engaged and none may be NOT engaged. The guard's enforcement does
#          not depend on its reporter, so silence after a boot confirmed is
#          a warning, repeated, never a stop -- the reporter died or the OS
#          hung, which this script cannot tell apart -- except after a reset:
#          libvirt's reboot event is watched, and after one the next boot
#          must report engaged within GUARD_SECS, as the first boot must
#          from the start (a reset the watch reads late is placed where the
#          report stood when it read it: the cautious side). A failure on a
#          "will" or "may" record asks the recovery OS to shut down (exit
#          6); on "no" it is a warning. Not seen: a new boot without a
#          reset that says nothing (kexec) -- it is only silence, a
#          warning. A host suspend, or a paused domain,
#          is not silence. The domain is started paused, afresh (never from
#          a managed-save image: one refuses the session), and resumed only
#          if its live definition carries the guard; otherwise it is
#          destroyed before it ran an instruction, and nothing boots.
#          Lifted inside the guest for the update (systemctl stop
#          das-vm-guard: pacman cannot replace a file something is mounted
#          on), engaged again after. Afterwards the domain is defined from
#          the template again; the template itself carries none of it.
#          status and session-end judge the report too, before session-end
#          removes anything, with the resets the session saw (one unanswered
#          is pending, then NOT confirmed) -- and status counts silence too,
#          since it cannot see a reset made after the session ended.
#   - This script never destroys a running recovery OS -- it could be in the
#     middle of an update. Every stop is an ACPI request (virsh shutdown),
#     sent again until it is off, and bounded (exit 3 at the bound). The one
#     exception, approved by the operator on 2026-10-05 (bd
#     DAS-Backup-Manager-0zm): `virsh destroy` of a domain started --paused
#     and never resumed, whose guest has not executed a single instruction --
#     no firmware, no OS, no write to the disk (started with --force-boot,
#     so never a restored image). destroy_never_resumed is the only call: it
#     refuses once `virsh resume` has been tried, and unless one read
#     immediately before says the domain is paused AND its vCPUs have never
#     run (their CPU time 0: paused alone could be a resume and a pause by
#     something else); a read that fails destroys nothing. A stop that does
#     not land within the bound is left to the operator, with the facts and
#     the trade-off. An
#     interrupt or an expired --timeout leaves the VM, the claim and the lock
#     as they are and says how to finish -- and what the guard was, or that
#     it was not judged.
#   - The host never writes the drive: no mount, no chroot, no copy. Every
#     write -- its ESP included -- is made by its own OS, booted in the VM.
#   - The VM's traffic goes direct (decision 5 of 2026-10-07): before the
#     domain starts, `ip rule add from <the subnet of libvirt's network
#     default> lookup main priority 5100` -- above a VPN exit node's lookup
#     (Tailscale's 5270), for that subnet only -- and it is taken out when the
#     disk is given back, on every way out that gives it back. A session left
#     running (exit 3) leaves it; session-end takes it out once no recovery
#     OS runs. One found in place (a session that did not finish) is taken
#     over and taken out.
#   - --unattended (decisions 1 and 3-8 of bd 8249): the update runs through
#     the recovery OS's own QEMU guest agent (virsh qemu-agent-command,
#     guest-exec; no SSH, no password), only when the boot record says the
#     agent is installed and started at boot, and never on a "will" record.
#     Its steps, each a transient unit in the recovery OS (it outlives a
#     restart of the agent): wait for the guard and the agent; egress -- the
#     VM's public address must belong to EGRESS_ORG, else nothing is updated;
#     snapshot -- the OS snapshots its own @ read-only to
#     @.pre-update.<YYYYMMDD-HHMM>, keeping the newest 2 (its own btrfs,
#     never the host's); guard-lift (systemctl stop das-vm-guard); keyrings;
#     upgrade (pacman -Syu --noconfirm); packages (qemu-guest-agent, and
#     amd-ucode intel-ucode reinstalled); initramfs (the fallback image added
#     to a preset that lacks it, then mkinitcpio -P); verify-btrbk (pacman
#     -Qkk btrbk: 0 altered files); verify-boot (every file the loader
#     entries name is on the ESP, every LABEL=/UUID= of /etc/fstab
#     resolves -- bd ac82, read inside the OS); guard-engage (started again,
#     every btrbk bound); reboot (a new boot whose guard reports engaged
#     and whose agent answers); kernel (uname -r); poweroff. Anything
#     unexpected stops it: the recovery OS is asked to power off (its agent,
#     then ACPI until it has; never destroyed), the disk is given back, exit 7.
#   - --wait-lock <minutes>: a scheduled session (the GUI's helper writes a
#     timer for it) may fire while a backup runs: instead of refusing at
#     once, wait up to that long for the target units to be inactive and the
#     maintenance lock to be free, looking every POLL_SECS, then go on as
#     usual (the lock is still taken without waiting: a job that slips in
#     between is refused there). Past the bound: refused, nothing held. A
#     two-drive run waits once, before taking the lock; its children do not.
#   - Both drives in one run (session A B --mode sequential|parallel): ONE
#     process holds the maintenance lock, once, and the egress rule, and runs
#     each drive's session as its child through that lock's descriptor;
#     sequential runs the second drive only after a first that exited 0, or
#     5 on warnings of its own session alone (decision 9, below); parallel
#     runs both at once.
#   - The session history: one JSON line per drive per session that took
#     the lock, in recovery-os-vm-history.jsonl beside the boot record;
#     `history` prints it and `clean-runs` counts a drive's consecutive clean
#     unattended sessions (any failed, kept, warned or overridden one resets
#     it to 0).
#   - console-socket: for a running session, the domain's VNC (libvirt's
#     root-only socket) offered through a socket of its own -- mode 0600, of
#     one user, in a fresh 0711 directory of root's under
#     /run/das-recovery-os-vm (0711) -- by socat, for one connection; taken
#     away when the disk is given back. No libvirt group, no ACL is changed.
#
# Usage (as root; history and clean-runs need only read access):
#   recovery-os-vm.sh define
#   recovery-os-vm.sh session <A|B|label> [--unattended] [--dry-run]
#                             [--timeout <minutes>] [--wait-lock <minutes>]
#                             [--accept-boot-record-risk]
#   recovery-os-vm.sh session <A|B|label> <A|B|label>
#                             [--mode sequential|parallel] [--unattended]
#                             [--dry-run] [--timeout <minutes>]
#                             [--wait-lock <minutes>]
#                             [--accept-boot-record-risk]
#   recovery-os-vm.sh session-end <A|B|label>
#   recovery-os-vm.sh status
#   recovery-os-vm.sh screenshot <A|B|label> <file.png>
#   recovery-os-vm.sh console-socket <A|B|label> <uid>
#   recovery-os-vm.sh history [<A|B|label>]
#   recovery-os-vm.sh clean-runs <A|B|label>
#
# Progress, for the GUI's helper to parse (one line each, on stdout; the
# label never holds a space):
#   PROGRESS <label> <step> <event>[ <message>]
#       step: preflight, start, wait, boot, egress, snapshot, guard-lift,
#       keyrings, upgrade, packages, initramfs, verify-btrbk, verify-boot,
#       guard-engage, reboot, kernel, poweroff, giveback; event: start, ok,
#       fail (the message says why). An attended session has preflight,
#       start, wait and giveback only.
#   OUTPUT <label> <step> <text>      a line a step printed in the recovery OS
#   RESULT <label> <exit> <outcome>   a drive's session ended (after it took
#                                     the lock): outcome clean (0), warnings
#                                     (5), kept (3, 4) or failed (any other)
#   DRIVE <label> <exit|skipped>      a two-drive run: each drive's end
#   Every other line is for people. console-socket prints only the socket's
#   path on stdout; clean-runs only the count.
#
# A and B are shorthands for the one role = "mirror" target whose label
# contains that letter as a dash-separated word (system-recovery-A-2tb).
#
# Exit status:
#   0  done
#   1  refused, failed or interrupted -- nothing is held (a started domain
#      without the guard is destroyed while still paused: nothing ran, a
#      retry is safe, and a repeat means a cause to fix; a session
#      interrupted as its recovery OS powered off gives everything back and
#      says what the guard was -- 5 if it did not confirm on a "no" record)
#   2  usage
#   3  the recovery OS is still running (an interrupt, --timeout without a
#      shutdown, or a shutdown asked for the guard and not done within
#      DAS_RECOVERY_VM_GRACE_SECS): the claim and the lock are KEPT; finish
#      with session-end. The message says first what the guard was: engaged,
#      NOT confirmed (power it off now), or not yet judged
#   4  the disk could not be returned completely (a detach, or the holder's
#      exit, failed): the claim and the lock are KEPT; finish with session-end
#   5  the session ended and the disk was given back, but something needs a
#      look (see the summary): the claim was lost while the VM ran (taken
#      again or not), the drive re-enumerated during the session, a
#      partition was mounted afterwards, the device scan failed, the
#      boot-record check was overridden (a dry run with the override too),
#      the session guard did not confirm on a "no" record, its reporter went
#      silent, lost lines or left its last one unfinished at power-off, time
#      was skipped between two looks (a host suspend, or this script
#      stopped: the guest may have run unwatched), the reset watch stopped,
#      or it could not be taken out of the domain's definition again
#   7  an unattended update stopped at a step (the summary and its PROGRESS
#      fail line name it): the recovery OS was asked to power off -- its
#      agent, and ACPI until it went; never destroyed -- and the disk was
#      given back; nothing after that step ran. Stopped at keyrings,
#      upgrade, packages or initramfs, the OS may be half-upgraded (the
#      power-off may end pacman mid-transaction): the summary names the
#      @.pre-update.<stamp> snapshot to roll back to. Not off within
#      DAS_RECOVERY_VM_GRACE_SECS: 3, as above
#   6  the session guard did not confirm on a "will" or "may" record: the
#      recovery OS was asked to shut down (ACPI, again until it went; never
#      destroyed) and powered off, or powered off by itself without
#      confirming -- a session interrupted as it did, too -- and the disk was
#      given back. status and session-end exit 6 (5 on a "no" record) when
#      the guard's report says NOT engaged, is missing for a recovery OS that
#      ran, cannot be read, shows a reset the session saw that no boot after
#      it answered (once shut off, or GUARD_SECS after it; pending before),
#      or -- status, for one not shut off -- has been silent for GUARD_SECS
#   A two-drive run exits with the gravest of its drives': 4, 3, 6, 7, 5,
#   then 1 (a drive skipped is not one), else 0; and 5 when its egress rule
#   could not be taken out. Sequential, the second drive starts after a
#   first that exited 0, or 5 for causes that stayed in its own session
#   only: the guard unconfirmed on a "no" record, its reporter's lost or
#   unfinished lines, the guard left in the domain's definition, the egress
#   rule not taken out, a dry run with --accept-boot-record-risk. A 5 for
#   the host, the enclosure or the mechanism -- the claim lost, the drive
#   re-enumerated, a partition mounted afterwards (or not known), the device
#   scan failed, time skipped, the reset watch stopped -- stops it, and so
#   does a 5 with no cause recorded, unreadable, or not in either list (a
#   real session's override, units the guard cannot mask, a silent
#   reporter). The warnings and the run's closing lines name the cause.
#   Any other exit (1, 3, 4, 6, 7) always stops it.
#
# Every step is logged to stdout and to the journal (tag das-recovery-os-vm).
#
# Test seams -- tests/test_recovery_os_vm.sh. Never set them in real use:
#   DAS_RECOVERY_VM_TEST_ROOT   prefix for every host path this script reads
#                               or writes (/run, /dev/disk/by-id, /sys,
#                               firmware). With it set, the lock taken is NOT
#                               the one backups and scrubs take; it is announced.
#   DAS_RECOVERY_VM_TEST_LOOP   a loop device (major 7, checked with stat)
#                               backed by a regular file, to lend instead of the
#                               drive. Only with a full target label; it
#                               replaces the drive's identity checks (serial,
#                               filesystem UUID) and nothing else.
#   DAS_RECOVERY_OS_STATE       the boot record to read instead (and the session
#                               times beside it). Only with the test hatch, and
#                               the hatch only with it: the record is what keeps
#                               a real drive's backups safe from its own OS, the
#                               hatch lends a loop file, and a hatch session must
#                               never count as a session of the real drive.
#   DAS_RECOVERY_VM_TEST_NO_EGRESS_RULE
#                               with the hatch only: put no egress rule in
#                               place (the unattended egress check's
#                               counter-test, on a real VM)
#   DAS_RECOVERY_VM_POLL_SECS (5), DAS_RECOVERY_VM_MINUTE_SECS (60),
#   DAS_RECOVERY_VM_GRACE_SECS (600), DAS_RECOVERY_VM_GUARD_SECS (900),
#   DAS_RECOVERY_VM_RESEND_SECS (20), DAS_RECOVERY_VM_CLOCK_GAP_SECS (120),
#   DAS_RECOVERY_VM_UPDATE_SECS (7200: the full upgrade's limit),
#   DAS_RECOVERY_VM_AGENT_SECS (GUARD_SECS: the agent's to answer after the
#   start), DAS_RECOVERY_VM_AGENT_GRACE_SECS (300: its silence while a step
#   runs)                       clocks
#   DAS_RECOVERY_VM_EGRESS_ORG  what the direct path egresses as (ipinfo.io's
#                               org line begins with it): AS209 by default,
#                               the author's ISP -- set it for yours
#   BTRDASD_BIN, DAS_CONFIG     as in backup-run.sh
# Set by a two-drive run for each drive's session, never by hand:
#   DAS_RECOVERY_VM_LOCK_FD (the run's descriptor of the maintenance lock),
#   DAS_RECOVERY_VM_MODE (sequential or parallel), DAS_RECOVERY_VM_EGRESS_HELD,
#   DAS_RECOVERY_VM_CAUSE_FILE (sequential: where the drive writes the causes
#   of its exit 5, one word a line, for the run to read)

set -euo pipefail
# One locale: bracket ranges then mean ASCII (bd 1bsx's class), and virsh
# says "shut off" -- what this script waits for -- in English.
export LC_ALL=C
# And no bracket range anyway (the tree's rule, bd 1bsx): digits are
# [[:digit:]], letters are spelt out.
readonly ASCII_LOWER=abcdefghijklmnopqrstuvwxyz
readonly ASCII_LETTERS=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ
# No job control: the holder must start in this process group, so that
# `setsid` gives it a session of its own without forking a second time.
set +m
# The holder's transient scope: <prefix>-<label>, -<n> for a replacement.
readonly HOLDER_UNIT_PREFIX="das-recovery-os-holder"

# One libvirt domain per role = "mirror" target (bd DAS-Backup-Manager-7wb,
# the operator's decision of 2026-10-04): recovery-os-updater-<label>, each
# with its own NVRAM, console, log and guard channel, rendered from the one
# template. DOMAIN is the domain of the drive a command is about
# (set_domain); LEGACY_DOMAIN is the one shared domain of before, which
# define retires.
readonly DOMAIN_BASE="recovery-os-updater"
readonly LEGACY_DOMAIN="$DOMAIN_BASE"
DOMAIN=""
readonly LIBVIRT_URI="qemu:///system"
# The NAT network the template puts the VM on: its subnet is what the egress
# rule routes (decision 5 of 2026-10-07).
readonly LIBVIRT_NETWORK="default"
readonly LOG_TAG="das-recovery-os-vm"
# Every unit that mounts the backup targets. Not named DAS_*: load_targets
# evals `btrdasd config dump-env`, which writes that namespace.
readonly TARGET_UNITS=(das-backup.service das-backup-full.service das-scrub.service das-backup-doctor.service)

SELF="$(readlink -f -- "${BASH_SOURCE[0]}")"
readonly SELF
SCRIPT_DIR="$(dirname -- "$SELF")"
readonly SCRIPT_DIR
BTRDASD_BIN="${BTRDASD_BIN:-/usr/bin/btrdasd}"
DAS_CONFIG="${DAS_CONFIG:-/etc/das-backup/config.toml}"

readonly TEST_ROOT="${DAS_RECOVERY_VM_TEST_ROOT:-}"
readonly TEST_LOOP="${DAS_RECOVERY_VM_TEST_LOOP:-}"
readonly POLL_SECS="${DAS_RECOVERY_VM_POLL_SECS:-5}"
readonly MINUTE_SECS="${DAS_RECOVERY_VM_MINUTE_SECS:-60}"
readonly GRACE_SECS="${DAS_RECOVERY_VM_GRACE_SECS:-600}"
# How long the recovery OS has, from its start, to report the session guard,
# and at most between two of its reports. Sized from the guide's own first
# boot: firmware and boot menu, a default entry that cannot find its root
# (a systemd initramfs waits 90 s for it), the operator rebooting into the
# fallback entry, and that boot -- a 5-minute limit races it.
readonly GUARD_SECS="${DAS_RECOVERY_VM_GUARD_SECS:-900}"
# How often a request to shut down is sent again until the recovery OS is
# off: one sent while it is in its firmware, boot menu or initramfs is
# dropped (QEMU drops a power-button press the guest has not enabled).
readonly RESEND_SECS="${DAS_RECOVERY_VM_RESEND_SECS:-20}"
# Two looks this far apart mean this script did not run in between -- a host
# suspend (the recovery OS did not run either), or this script stopped: the
# guard's clock does not count that time. A look takes at most about 45 s
# (a holder claimed again), so this is well above any look's own length.
readonly CLOCK_GAP_SECS="${DAS_RECOVERY_VM_CLOCK_GAP_SECS:-120}"
# Unattended sessions: how long the full upgrade may take, how long the guest
# agent has to answer after the start, and how long it may be silent while a
# step runs (pacman restarting it, or systemd re-executing).
readonly UPDATE_SECS="${DAS_RECOVERY_VM_UPDATE_SECS:-7200}"
readonly AGENT_SECS="${DAS_RECOVERY_VM_AGENT_SECS:-$GUARD_SECS}"
readonly AGENT_GRACE_SECS="${DAS_RECOVERY_VM_AGENT_GRACE_SECS:-300}"
# What the VM's direct path egresses as, the start of ipinfo.io's org line
# (decision 5 of 2026-10-07): the author's ISP by default, AS209.
readonly EGRESS_ORG="${DAS_RECOVERY_VM_EGRESS_ORG:-AS209}"
# The egress rule's priority: above Tailscale's lookup 52 (5270).
readonly EGRESS_PRIORITY=5100

# Installed side by side by CMake: ${prefix}/lib/das-backup/{this script,libvirt/}.
# The template; each drive's definition is rendered from it (render_domain_xml)
# into DOMAIN_XML, beside the session's other state.
readonly DOMAIN_TEMPLATE="$SCRIPT_DIR/libvirt/$DOMAIN_BASE.xml"
DOMAIN_XML=""
# Must match indexer/src/scrub.rs MAINTENANCE_LOCK_PATH and backup-run.sh.
readonly MAINTENANCE_LOCK="$TEST_ROOT/run/das-maintenance.lock"
readonly STATE_DIR="$TEST_ROOT/run/das-recovery-os-vm"
# The egress rule's subnet, while a rule this script put in place is there.
readonly EGRESS_FILE="$STATE_DIR/egress.rule"
readonly BY_ID="$TEST_ROOT/dev/disk/by-id"
readonly SYS_BLOCK="$TEST_ROOT/sys/block"

# The boot record (bd DAS-Backup-Manager-1yg): the schemas this reads (4 adds
# the guest agent, which only --unattended needs), and the age past which it
# no longer describes the drive (the nightly run rewrites it daily; a week of
# misses is not a record of today's OS).
readonly OS_STATE_SCHEMAS="3 4"
readonly MAX_RECORD_AGE_DAYS=8
# One line per fact; every string cleaned of control characters, since unit
# names and reasons come from the recovery OS itself. A runner is said by the
# name the guard would mask: the unit that runs btrbk, or for a cron file
# (a source with a slash) the cron daemon that runs it.
# shellcheck disable=SC2016 # jq's variables, not the shell's
readonly BOOT_RECORD_JQ='def clean: tostring | gsub("[[:cntrl:]]"; "?");
"schema\t\(.schema_version | clean)",
"schematype\t\(.schema_version | type)",
(if .schema_version == 3 or .schema_version == 4 then
   .drives[$label] as $d
   | if $d == null then "entry\tnone"
     else
       "entry\tpresent",
       "checked\t\($d.checked_epoch | clean)",
       "checkedtype\t\($d.checked_epoch | type)",
       "fs\t\(($d.mount_uuid // "") | clean)",
       "fstype\t\($d.mount_uuid | type)",
       (if $d.error != null then "error\t\(($d.error | clean) as $e | if $e == "" then "an error without text" else $e end)"
        elif $d.os == null then "error\tno OS was inspected"
        else empty end),
       (if $d.os == null then empty else
          "verdict\t\(($d.os.btrbk_at_boot.verdict // "") | clean)",
          (($d.os.btrbk_at_boot.reasons // [])[] | "reason\t\(clean)"),
          (($d.os.btrbk_at_boot.runners // [])[]
           | (if ((.source // "") | tostring | contains("/")) then .via else .source end) // ""
           | "runner\t\(clean)"),
          ($d.os.guest_agent as $g
           | if $g == null then "agent\tnot recorded (a record of schema 3, made before it was read)"
             elif $g.state == "read" then "agent\tread \($g.installed == true) \($g.enabled == true)", "agentwhy\t\(($g.why // "") | clean)"
             else "agent\t\(($g.state // "unknown") | clean)", "agentwhy\t\(($g.reason // "") | clean)" end),
          ($d.os.enabled_units as $u
           | if $u == null then "units\tnot recorded"
             elif $u.state == "listed" then "units\tlisted", (($u.units // [])[] | "unit\t\(.name | clean)")
             else "units\t\(($u.state // "unknown") | clean)\(if $u.reason then ": " + ($u.reason | clean) else "" end)" end)
        end)
     end
 else empty end)'

# The session guard (bd DAS-Backup-Manager-0zm): its two units, the drop-in
# that pulls both into the boot, the virtio port its report goes through,
# where btrbk can be (both covered when present), what is always masked, and
# the names a mask is ever given -- the characters unit names are made of,
# starting with a letter or digit, and only units that can run something.
readonly GUARD_UNIT="das-vm-guard.service"
readonly GUARD_REPORT_UNIT="das-vm-guard-report.service"
readonly GUARD_DROPIN="sysinit.target~das-vm-guard"
readonly GUARD_PORT="org.dasbackup.guard"
# Every directory a PATH lookup visits for btrbk on a usr-merged or split
# system; the record names the units that run btrbk, not the binary each one
# resolves to (schema 3), so these are covered whatever the record says.
readonly GUARD_BTRBK_PATHS=(/usr/bin/btrbk /usr/local/bin/btrbk /usr/local/sbin/btrbk /usr/sbin/btrbk /bin/btrbk /sbin/btrbk)
readonly GUARD_DEFAULT_MASKS=(btrbk.service btrbk.timer cronie.service crond.service)
# Two strings a mask: 4 of the guard's own + 2 x 64 = 132, inside the 255
# SMBIOS OEM strings one structure can count.
readonly GUARD_MAX_MASKS=64
readonly GUARD_ISSUE="/run/issue.d/das-vm-guard.issue"
# What runs in place of btrbk, and the credential that carries it.
readonly GUARD_STUB="/run/das-vm-guard/btrbk"
readonly GUARD_STUB_CRED="das-vm-guard.btrbk"
# Each mask's drop-in: named to sort after any an administrator writes
# (systemctl edit writes override.conf), with a condition that can never hold
# -- nothing can exist under /dev/null.
readonly GUARD_MASK_DROPIN="zzzzzzzz-das-vm-guard"
readonly GUARD_NEVER="/dev/null/das-vm-guard"
# Where systemd-debug-generator writes extra units and drop-ins (it runs with
# them ahead of /etc and /usr), and how often the recovery OS reports.
readonly GUARD_EARLY="/run/systemd/generator.early"
readonly GUARD_HEARTBEAT_SECS=60
# Unit names, ASCII spelled out: a range ([A-Z]) follows the locale's
# collation, and systemd refuses ":" in a credential's name.
readonly UNIT_NAME_RE='^[abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789][abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_.@-]*\.(service|timer|socket|path)$'
# One line of the guard's report: what, then " seq " and its number in this
# boot (from 1), then " boot " and the boot's id.
readonly REPORT_LINE_RE='^(.*) seq ([123456789][0123456789]{0,8}) boot ([0123456789abcdef]{8}-[0123456789abcdef]{4}-[0123456789abcdef]{4}-[0123456789abcdef]{4}-[0123456789abcdef]{12})$'
# What libvirt's event stream says when the domain is reset -- a reboot
# inside it, or virsh reset (virsh-domain-event.c, the generic print).
RESET_EVENT=""
# How many rotated files of the report virtlogd keeps (.0 the newest) are
# looked for at most; its max_backups is 3 by default.
readonly REPORT_BACKUPS_MAX=10
# A reason that begins with what runs: "unit ..." or "timer starts unit, which ...".
readonly REASON_UNITS_RE='^([^ ]+)( starts ([^ ]+), which )?'

# How long the holder has to announce its claim (a spun-down disk may take
# seconds to open), and to exit after SIGTERM. In tenths of a second.
readonly HOLDER_START_TICKS=300
readonly HOLDER_STOP_TICKS=100

# Patterns, kept in variables so that =~ treats them as regular expressions.
readonly NAME_RE="<name>([^<]+)</name>"
readonly LOADER_RE="<loader [^>]*>([^<]+)</loader>"
readonly TEMPLATE_RE="<nvram template='([^']+)'"
readonly SOURCE_RE="<source (dev|file)='([^']*)'"
readonly HELD_RE="^held (.+) pid ([[:digit:]]+)$"
# The template's lines each drive's domain makes its own (render_domain_xml).
readonly UUID_LINE_RE="^  <uuid>[^<]+</uuid>$"
readonly NVRAM_LINE_RE="^( +<nvram template='[^']+'>)[^<]+(</nvram>)$"
readonly MAC_LINE_RE="^( +)<mac address='[^']+'/>$"
# libvirt's network address, and the live domain's VNC socket.
readonly NET_ADDR_RE="address='([^']+)'"
readonly NET_PREFIX_RE="prefix='([[:digit:]]+)'"
readonly NET_MASK_RE="netmask='([^']+)'"
readonly IPV4_RE="^[[:digit:]]{1,3}\\.[[:digit:]]{1,3}\\.[[:digit:]]{1,3}\\.[[:digit:]]{1,3}$"
readonly VNC_SOCKET_RE="socket='([^']+)'"

# ---------------------------------------------------------------------------
# Session state, read by the EXIT trap
# ---------------------------------------------------------------------------
LABEL=""            # the target's config label
LABEL_EXPLICIT=false # given in full, not as a one-letter shorthand
SERIAL=""
DISK=""             # the whole-disk path lent to the VM (by-id, or the test loop)
DISK_DEV=""         # what it resolved to (/dev/sdX) when the session began
HOLDER_FILE=""      # record: holder pid, the disk path, the holder's scope
HOLDER_OUT=""       # the holder's stdout (its one `held` line)
HOLDER_ERR=""       # the holder's stderr
DISK_XML_FILE=""
LOCK_FD=""          # set while THIS process holds the maintenance lock's descriptor
HOLDER_PID=""
HOLDER_STARTING=false
HOLDER_SIGNALLED=false # stop_holder found it running and sent SIGTERM
HOLDER_FAILURE=""   # why start_holder failed
HOLDER_LOSSES=0     # holders that died while the VM may have used the disk
HOLDER_STARTS=0     # holders started this session (names their scopes)
HOLDER_UNIT=""      # the current holder's scope unit
REENUMERATED=false  # the by-id link led elsewhere during the session
CLAIM_GAP=false     # the claim was lost and no claim holds yet
HOLDER_RESULT="no holder left to stop" # what giving the disk back did with the holder
OS_STATE_FILE="$TEST_ROOT/var/lib/das-backup/recovery-os.json" # the boot record
SESSIONS_FILE=""    # the session times, beside the boot record
SESSIONS_FAILURE="" # why they could not be read or written
LAST_SESSION=""     # this label's last session time, if one is recorded
ACCEPT_BOOT_RISK=false
BOOT_OVERRIDES=()   # what --accept-boot-record-risk let through
VERDICT=""          # the boot record's btrbk-at-boot verdict: will, may or no
REC_UNITS=()        # the record's enabled units, when it lists them
REC_REASONS=()      # what its verdict rests on
REC_RUNNERS=()      # the name each runner it found would be masked by
MASKS=()            # the units the session guard masks
GUARD_ENTRIES=()    # the SMBIOS strings that carry the guard
GUARD_EXPECT=""     # the one line a recovery OS whose guard holds reports
GUARD_EXPECT_C=""   # ...in short: the mask list as its digest
GUARD_EXPECT_LIFTED_C=""
GUARD_FILE=""       # where that report lands: the port's file on this host
RESET_FILE=""       # the resets the reset watch saw, with where the report stood
RESET_WATCH_PID=""  # the reset watch: a reader of libvirt's event stream
RESET_VIRSH_PID=""  # the virsh whose event stream it reads: this script's child
RESET_VIRSH_STARTING=false # set just before that virsh starts: on_exit uses $!
RESET_VIRSH_BEFORE=""      # $! before that virsh started
RESET_LINES_READ=0  # lines of RESET_FILE already taken
RESET_WATCH_DOWN="" # why the reset watch is not running, once it stopped
RESETS=0            # resets of the VM seen this session
DESTROYED=false     # destroy_never_resumed destroyed the domain
DESTROY_STATE=""    # what destroy_never_resumed read the domain's state as
MAY_HAVE_RUN=false  # ...and found it resumed by something else (not paused, or its vCPUs ran)
GUARD_XML_FILE=""   # the domain definition with the guard, as defined
GUARDED=false       # the guard MAY be in the domain's definition (set before defining)
GUARD_CONFIRMED=false
GUARD_FAILED=false  # it did not confirm (GUARD_RESULT says how)
GUARD_SHUT_DOWN=false # ...and the recovery OS was asked to shut down for it
GUARD_RESULT="not booted"
GUARD_LEFT=""       # why the guard could not be taken out of the definition
GUARD_EXPECT_LIFTED="" # the line of a recovery OS whose guard is lifted for the update
GUARD_STATE_FILE="" # the session's guard state, for status and session-end
UNMASKABLE=()       # what the record names that the guard cannot mask
# The guard's clock: seconds the recovery OS could run, counted from its
# resume -- not while it is paused, not across a gap between two looks.
GCLOCK=0
G_TICK_AT=0         # SECONDS at the last look
G_LAST_LINE=0       # GCLOCK when the last report line arrived
G_GAPS=0            # gaps between two looks (a host suspend, or this script stopped), unless
G_GAP_SECS=0        # the domain read paused on both sides -- and the time they took
G_TICK_STATE=""     # the domain's state at the last look
# What must still prove itself: "start" (the first boot) or "reset" (the boot
# after one), from a boot whose first line comes at or after PROOF_POS in
# the report, before GCLOCK reaches PROOF_DEADLINE. Empty: nothing.
PROOF_WHY=""
PROOF_POS=0
PROOF_DEADLINE=0
PROOF_AT=0          # when the first reset not answered was (seconds since the epoch)
PROOF_ANSWERED=""   # what the line just judged answered: start, reset, or nothing
SILENT=false        # silent past GUARD_SECS now, with no reset since (a warning)
SILENT_NEXT=0       # GCLOCK at which that is said again
SILENCES=0          # how many times it went silent
SILENCE_LONGEST=0
G_WATCH_DOWN_AT=0   # GCLOCK when the reset watch was found stopped
G_LIFTS_SAID=0      # lifts already said
REPORT_LOSSES=()    # what of the report was lost (cut, or gone before it was read)
SHUTDOWN_ASKED=0    # how often the recovery OS was asked to shut down
SHUTDOWN_SECS=0     # ...over how long
RESUME_ATTEMPTED=false # `virsh resume` was tried: never destroy from here on
ATTACHED=false      # the disk MAY be in the domain's definition (set before attaching)
STARTED=false       # `virsh start` was attempted, so the guest may have written
DONE=false          # nothing left for the trap to undo
KEEP_REASON=""
RETURN_FAILURE=""
SCAN_RESULT="not needed"
MOUNT_RESULT="not checked"
DRY_RUN=false
TIMEOUT_MIN=""
WAIT_LOCK_MIN="" # session --wait-lock
TARGET_ARG=""
SESSION_START=0
VM_START=0
SESSION_EPOCH=0
UNATTENDED=false    # session --unattended
RUN_MODE=single     # single, or a two-drive run's: sequential or parallel
PAIR_MODE=sequential
TARGET_ARG2=""      # the second drive of a two-drive run
PAIR_DONE=false PAIR_KEPT=false PAIR_RCS=() PAIR_LABELS=()
# The maintenance lock's descriptor came from a two-drive run that holds it.
LOCK_INHERITED=false
EGRESS_HELD=false   # ...and so does its egress rule
CAUSE_FILE=""       # ...and where a sequential one reads this drive's exit-5 causes
EGRESS_OWNED=false  # this process put the egress rule in place (or took one over)
EGRESS_SUBNET="" EGRESS_COUNT=0 EGRESS_FAILURE=""
EGRESS_RESULT="not needed"
HISTORY_FILE=""     # the session history, beside the boot record
HISTORY_ARMED=false # this session took the lock: its end is written there
HISTORY_FAILURE="" HISTORY_LINES=""
# An attended session of a "will" drive (decision 3 of bd 8249).
WILL_BANNER=false
readonly WILL_BANNER_TEXT="this OS runs btrbk at boot; the guard is stopping it now; disable it in this session (mask the unit the boot record names), then let the next backup run record the drive again"
REC_AGENT=""        # the record's guest agent: "read <installed> <enabled>", or why not
REC_AGENT_WHY=""
UNATTENDED_FAILED="" UNATTENDED_STAGE="" UNATTENDED_WHY="" UNATTENDED_DEADLINE=0
UNATTENDED_HALF=""  # said when a step that changes the OS was stopped
PRE_UPDATE_SNAPSHOT=""  # the snapshot step's @.pre-update.<stamp>
KERNEL=""           # what an unattended session's reboot came back with
STAGE_N=0 OUT_PART="" OUT_LAST=""
GSH_RC="" GSH_OUT="" GSH_WHY="" AGENT_OUT=""
CONSOLE_FILE=""     # the console bridge's record: pid, directory
RENDER_FAILURE=""
D_UUID="" D_MAC=""
LOG_PREFIX=""       # "[label] " in a drive's session of a two-drive run
LOG_TO_STDERR=false # console-socket prints only the socket path on stdout

# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------
log() {
    if [[ "$LOG_TO_STDERR" == true ]]; then
        printf '%s%s\n' "$LOG_PREFIX" "$*" >&2
    else
        printf '%s%s\n' "$LOG_PREFIX" "$*"
    fi
    logger -t "$LOG_TAG" -- "$LOG_PREFIX$*" || :
}

warn() {
    printf '%sWARNING: %s\n' "$LOG_PREFIX" "$*" >&2
    logger -p user.warning -t "$LOG_TAG" -- "${LOG_PREFIX}WARNING: $*" || :
}

refuse() {
    printf '%sREFUSED: %s\n' "$LOG_PREFIX" "$*" >&2
    logger -p user.err -t "$LOG_TAG" -- "${LOG_PREFIX}REFUSED: $*" || :
    exit 1
}

usage_text() {
    cat <<EOF
Usage: $(basename -- "$SELF") COMMAND   (as root; history and clean-runs read only)

Boot one recovery drive's own OS in a libvirt domain of its own
($DOMAIN_BASE-<label>), with the whole disk passed through, to update it
without rebooting the workstation.

  define                 define or update every recovery drive's domain from
                         $DOMAIN_TEMPLATE,
                         and retire the shared $LEGACY_DOMAIN of before (each
                         shut off, with no disk attached)
  session <A|B|label> [--unattended] [--dry-run] [--timeout <minutes>]
          [--wait-lock <minutes>] [--accept-boot-record-risk]
                         lend one role = "mirror" drive to its VM, boot it
                         with the session guard (btrbk cannot run in it),
                         wait until it powers off, give the disk back --
                         only when the nightly run's record is of the
                         drive's filesystem (mount_uuid), is fresh, and is
                         newer than the drive's last session (the flag lets
                         an age or a session through, loudly); a record
                         saying its OS will run btrbk at boot goes on with
                         a banner: disable it in the session.
                         --unattended: the update itself, through the
                         recovery OS's guest agent (its record must say the
                         agent runs at boot; never on "will")
                         --wait-lock: wait up to that many minutes for a
                         running backup or scrub to finish, instead of
                         refusing at once
  session <A|B|label> <A|B|label> [--mode sequential|parallel] [...]
                         both drives in one run, under one lock: one after
                         the other (the default; the second only after a
                         first that exited 0, or 5 on its own session's
                         warnings alone), or at once
  session-end <A|B|label>
                         finish a session whose driver died (VM shut off);
                         judges the guard's report before removing anything
  status                 each drive's domain, attached disk and guard;
                         holder, lock, egress rule
  screenshot <A|B|label> <file.png>
                         that drive's VM screen as a PNG, while it runs
  console-socket <A|B|label> <uid>
                         for a running session: its VNC through a socket
                         only user <uid> can open, once; prints its path
  history [<A|B|label>]  the session history, one JSON line per session
  clean-runs <A|B|label> consecutive clean unattended sessions of the drive

Exit status: 0 done; 1 refused, failed or interrupted, nothing held;
2 usage; 3 the recovery OS is still running and keeps the disk and the lock;
4 the disk could not be returned completely and is kept (finish 3 and 4 with
session-end); 5 done and given back, but see the summary's warnings (a dry
run that needed --accept-boot-record-risk exits 5 too); 6 the session guard
did not confirm on a "will" or "may" record, so the recovery OS was shut down
(never destroyed) and the disk given back; 7 an unattended update stopped at
a step (named), the recovery OS was powered off and the disk given back.
status and session-end exit 6 (5 on a "no" record) when the guard's report
says it is not engaged, cannot be judged, shows a reset its session saw that
no boot after it answered in time, or -- for a recovery OS not shut off --
has been silent too long. Progress lines (PROGRESS, OUTPUT, RESULT, DRIVE):
see the script's header.
EOF
}

usage() {
    usage_text >&2
    exit 2
}

format_duration() {
    local s=$1
    printf '%dh %02dm %02ds' $((s / 3600)) $((s % 3600 / 60)) $((s % 60))
}

# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------
virsh_() {
    virsh --connect "$LIBVIRT_URI" "$@"
}

require_root() {
    if [[ "$(id -u)" != 0 ]]; then
        refuse "must run as root: sudo $SELF ..."
    fi
}

# A path that goes into a libvirt definition and onto command lines: only the
# characters udev itself uses in /dev/disk/by-id names.
check_path_chars() {
    if [[ ! "$1" =~ ^/[${ASCII_LETTERS}[:digit:]/#+.:=@_-]+$ ]]; then
        refuse "the path '$1' has characters this script will not put in a libvirt definition"
    fi
}

# The first capture of regular expression $1 on a line of file $2.
xml_value() {
    local line
    while IFS= read -r line; do
        if [[ "$line" =~ $1 ]]; then
            printf '%s\n' "${BASH_REMATCH[1]}"
            return 0
        fi
    done <"$2"
    return 1
}

# Every disk's source in a domain definition, one per line ("(no source)"
# for one without). libvirt prints one element per line.
disk_sources() {
    local line in_disk=false src=""
    while IFS= read -r line; do
        if [[ "$line" == *"<disk "* ]]; then
            in_disk=true
            src="(no source)"
        fi
        if [[ "$in_disk" == true && "$line" =~ $SOURCE_RE ]]; then
            src=${BASH_REMATCH[2]}
        fi
        if [[ "$in_disk" == true && "$line" == *"</disk>"* ]]; then
            printf '%s\n' "$src"
            in_disk=false
        fi
    done <<<"$1"
}

# Whether $1 is in the domain's persistent definition. When that cannot be
# read the answer is yes: the cautious one, since "no" lets the lock go.
disk_in_definition() {
    local xml src
    if ! xml="$(virsh_ dumpxml --inactive "$DOMAIN" 2>&1)"; then
        warn "cannot read $DOMAIN's definition: $xml"
        return 0
    fi
    while IFS= read -r src; do
        if [[ "$src" == "$1" ]]; then
            return 0
        fi
    done < <(disk_sources "$xml")
    return 1
}

current_state() {
    local s
    if s="$(virsh_ domstate "$DOMAIN" 2>&1)"; then
        printf '%s\n' "$s"
    else
        printf 'unknown (virsh: %s)\n' "$s"
    fi
}

# "sdj2 on /mnt/x, ..." for every mounted partition of disk $1 (empty when
# none); fails when lsblk does, so nothing unread passes for "unmounted".
mounted_partitions() {
    local out name mps list=""
    out="$(lsblk -nr -o NAME,MOUNTPOINTS -- "$1" 2>&1)" || return 1
    while read -r name mps; do
        if [[ -n "$mps" ]]; then
            list+="${list:+, }$name on ${mps//\\x0a/ and }"
        fi
    done <<<"$out"
    printf '%s' "$list"
}

# Partition $2 of whole disk $1: by-id names take -partN, devices ending in
# a digit (loop0, nvme0n1) take pN, the rest take N.
partition_path() {
    if [[ "$1" == */disk/by-id/* ]]; then
        printf '%s-part%s\n' "$1" "$2"
    elif [[ "$1" =~ [[:digit:]]$ ]]; then
        printf '%sp%s\n' "$1" "$2"
    else
        printf '%s%s\n' "$1" "$2"
    fi
}

# The holder record: line 1 the pid, line 2 the disk, line 3 the holder's
# scope unit (absent in a record of a holder started some other way).
read_record() {
    REC_PID=""
    REC_DEV=""
    REC_UNIT=""
    { IFS= read -r REC_PID && IFS= read -r REC_DEV; } <"$1" 2>/dev/null || return 1
    { IFS= read -r _ && IFS= read -r _ && IFS= read -r REC_UNIT; } <"$1" 2>/dev/null || REC_UNIT=""
    [[ "$REC_PID" =~ ^[[:digit:]]+$ && -n "$REC_DEV" ]]
}

write_record() {
    local tmp="$HOLDER_FILE.tmp"
    (umask 077 && printf '%s\n%s\n%s\n' "$1" "$DISK" "$HOLDER_UNIT" >"$tmp")
    mv -f -- "$tmp" "$HOLDER_FILE"
}

# Whether pid $1 is a running `btrdasd recovery-os hold-disk --device $2`.
# A zombie has an empty command line, so it does not count; neither does a
# recycled pid running something else.
holder_alive() {
    local cmd
    [[ "$1" =~ ^[[:digit:]]+$ ]] || return 1
    # stderr first: a gone pid fails the < redirection, which would print.
    cmd="$(tr '\0' ' ' 2>/dev/null <"/proc/$1/cmdline")" || return 1
    [[ "$cmd" == *" recovery-os hold-disk --device $2 "* ]]
}

# SIGTERM the holder and wait until it has gone; 1 if it will not go.
# HOLDER_SIGNALLED says whether it was still there to be told.
stop_holder() {
    local i
    HOLDER_SIGNALLED=false
    holder_alive "$1" "$2" || return 0
    HOLDER_SIGNALLED=true
    kill -TERM "$1" 2>/dev/null || :
    for ((i = 0; i < HOLDER_STOP_TICKS; i++)); do
        holder_alive "$1" "$2" || return 0
        sleep 0.1
    done
    return 1
}

# The lock, said in one line: free, or held and by whom (its first line,
# which every holder writes). The probe takes the lock for an instant.
lock_report() {
    local first=""
    if [[ ! -e "$MAINTENANCE_LOCK" ]]; then
        printf 'free (not taken since boot)\n'
        return 0
    fi
    first="$(head -n 1 -- "$MAINTENANCE_LOCK" 2>/dev/null)" || first=""
    if (flock -n 9) 9<"$MAINTENANCE_LOCK"; then
        printf 'free%s\n' "${first:+ (last holder: $first)}"
    else
        printf 'held by: %s\n' "${first:-(no holder line)}"
    fi
}

disk_xml() {
    cat <<EOF
<disk type='block' device='disk'>
  <driver name='qemu' type='raw' cache='none' io='native'/>
  <source dev='$1'/>
  <target dev='sda' bus='sata'/>
  <boot order='1'/>
</disk>
EOF
}

# ---------------------------------------------------------------------------
# Which drive
# ---------------------------------------------------------------------------

# The targets from config.toml, through the same export backup-run.sh uses.
load_targets() {
    local env_text i label_var role_var serials_var serial_var uuid_var label
    if ! env_text="$("$BTRDASD_BIN" config dump-env --config "$DAS_CONFIG")"; then
        refuse "btrdasd could not read $DAS_CONFIG"
    fi
    eval "$env_text"
    TARGET_LABELS=()
    declare -gA T_ROLE=()
    declare -gA T_SERIALS=()
    declare -gA T_UUIDS=()
    for ((i = 0; i < DAS_TARGET_COUNT; i++)); do
        label_var="DAS_TARGET_${i}_LABEL"
        role_var="DAS_TARGET_${i}_ROLE"
        serials_var="DAS_TARGET_${i}_SERIALS"
        serial_var="DAS_TARGET_${i}_SERIAL"
        uuid_var="DAS_TARGET_${i}_MOUNT_UUID"
        label="${!label_var}"
        TARGET_LABELS+=("$label")
        T_ROLE[$label]="${!role_var:-}"
        T_SERIALS[$label]="${!serials_var:-${!serial_var:-}}"
        T_UUIDS[$label]="${!uuid_var:-}"
    done
}

mirror_labels() {
    local label out=""
    for label in "${TARGET_LABELS[@]}"; do
        if [[ "${T_ROLE[$label]}" == mirror ]]; then
            out+="${out:+, }$label"
        fi
    done
    printf '%s' "${out:-none}"
}

# $1 is a label, or one letter naming the one mirror target whose label has
# it as a dash-separated word. Sets LABEL and LABEL_EXPLICIT.
resolve_label() {
    local arg=$1 label matches=()
    if [[ -n "${T_ROLE[$arg]+set}" ]]; then
        LABEL=$arg
        LABEL_EXPLICIT=true
    elif [[ "$arg" =~ ^[${ASCII_LETTERS}]$ ]]; then
        arg=${arg^^}
        for label in "${TARGET_LABELS[@]}"; do
            if [[ "${T_ROLE[$label]}" == mirror && "-$label-" == *"-$arg-"* ]]; then
                matches+=("$label")
            fi
        done
        if ((${#matches[@]} != 1)); then
            refuse "'$1' matches ${#matches[@]} role = \"mirror\" targets (${matches[*]:-none}) -- give the label (mirror targets: $(mirror_labels))"
        fi
        LABEL=${matches[0]}
        LABEL_EXPLICIT=false
    else
        refuse "no target labelled '$arg' in $DAS_CONFIG (mirror targets: $(mirror_labels))"
    fi
    if [[ "${T_ROLE[$LABEL]}" != mirror ]]; then
        refuse "'$LABEL' is a role = \"${T_ROLE[$LABEL]}\" target -- only a role = \"mirror\" recovery drive is ever lent to the VM"
    fi
    HOLDER_FILE="$STATE_DIR/$LABEL.holder"
    HOLDER_OUT="$STATE_DIR/$LABEL.holder.out"
    HOLDER_ERR="$STATE_DIR/$LABEL.holder.err"
    DISK_XML_FILE="$STATE_DIR/$LABEL.disk.xml"
    GUARD_FILE="$STATE_DIR/$LABEL.guard"
    GUARD_XML_FILE="$STATE_DIR/$LABEL.domain.xml"
    GUARD_STATE_FILE="$STATE_DIR/$LABEL.guard.state"
    RESET_FILE="$STATE_DIR/$LABEL.resets"
    CONSOLE_FILE="$STATE_DIR/$LABEL.console"
    set_domain "$LABEL"
}

# The domain of drive $1: recovery-os-updater-<label>. Its definition is
# rendered from the template into DOMAIN_XML (render_domain_xml) when a
# command needs it.
set_domain() {
    if [[ ! "$1" =~ ^[${ASCII_LETTERS}[:digit:]._-]+$ ]]; then
        refuse "the label '$1' has characters this script will not put in a libvirt domain's name (letters, digits, '.', '_' and '-' only)"
    fi
    DOMAIN="$DOMAIN_BASE-$1"
    DOMAIN_XML="$STATE_DIR/$1.plain.xml"
    # What libvirt's event stream says when the domain is reset -- a reboot
    # inside it, or virsh reset (virsh-domain-event.c, the generic print).
    RESET_EVENT="event 'reboot' for domain '$DOMAIN'"
}

# The fixed identity of drive $1's domain, derived from its label so that
# every define gives the same one (libvirt refuses to redefine a name under
# another UUID; a new MAC is a new network card to the recovery OS): D_UUID
# and D_MAC, from the SHA-256 of the label.
domain_identity() {
    local h
    h="$(printf 'das-backup %s %s' "$DOMAIN_BASE" "$1" | sha256sum)" || refuse "cannot derive the identity of $1's domain"
    h=${h:0:64}
    [[ "$h" =~ ^[0123456789abcdef]{64}$ ]] || refuse "cannot derive the identity of $1's domain"
    D_UUID="${h:0:8}-${h:8:4}-8${h:13:3}-a${h:17:3}-${h:20:12}"
    D_MAC="52:54:00:${h:32:2}:${h:34:2}:${h:36:2}"
}

# Drive $LABEL's definition, rendered from the template into DOMAIN_XML: its
# name, UUID, NVRAM file and MAC address are its own; nothing else differs.
# Each line replaced must be in the template exactly once, and the network
# must be LIBVIRT_NETWORK, or nothing is written: 1, with RENDER_FAILURE set.
# Never a refusal itself: giving a disk back renders it too, and must go on.
render_domain_xml() {
    local line n_name=0 n_uuid=0 n_nvram=0 n_mac=0 n_net=0 out
    RENDER_FAILURE=""
    if [[ ! -r "$DOMAIN_TEMPLATE" ]]; then
        RENDER_FAILURE="the domain template $DOMAIN_TEMPLATE is missing -- install the project (cmake --install) first"
        return 1
    fi
    domain_identity "$LABEL"
    out=""
    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" == "  <name>$DOMAIN_BASE</name>" ]]; then
            n_name=$((n_name + 1)) line="  <name>$DOMAIN</name>"
        elif [[ "$line" =~ $UUID_LINE_RE ]]; then
            n_uuid=$((n_uuid + 1)) line="  <uuid>$D_UUID</uuid>"
        elif [[ "$line" =~ $NVRAM_LINE_RE ]]; then
            n_nvram=$((n_nvram + 1)) line="${BASH_REMATCH[1]}/var/lib/libvirt/qemu/nvram/${DOMAIN}_VARS.fd${BASH_REMATCH[2]}"
        elif [[ "$line" =~ $MAC_LINE_RE ]]; then
            n_mac=$((n_mac + 1)) line="${BASH_REMATCH[1]}<mac address='$D_MAC'/>"
        elif [[ "$line" == "  <title>DAS recovery OS updater</title>" ]]; then
            line="  <title>DAS recovery OS updater: $LABEL</title>"
        fi
        [[ "$line" != *"<source network='$LIBVIRT_NETWORK'/>"* ]] || n_net=$((n_net + 1))
        out+="$line"$'\n'
    done <"$DOMAIN_TEMPLATE"
    if ((n_name != 1 || n_uuid != 1 || n_nvram != 1 || n_mac != 1 || n_net != 1)); then
        RENDER_FAILURE="$DOMAIN_TEMPLATE is not of the shape this script renders each drive's domain from: <name>$DOMAIN_BASE</name>, <uuid>, <nvram>, one <mac> and <source network='$LIBVIRT_NETWORK'/> each once (found $n_name, $n_uuid, $n_nvram, $n_mac, $n_net)"
        return 1
    fi
    if ! make_state_dir || ! (umask 077 && printf '%s' "$out" >"$DOMAIN_XML.new" && mv -f -- "$DOMAIN_XML.new" "$DOMAIN_XML"); then
        rm -f -- "$DOMAIN_XML.new"
        RENDER_FAILURE="cannot write $DOMAIN_XML"
        return 1
    fi
}

# The session state directory: root's, 0711 -- nothing in it can be listed
# by anyone else, and every file in it is root's and 0600, but a console
# bridge's own directory in it (console-socket) must be reachable by the user
# it is made for.
make_state_dir() {
    install -d -m 0711 -- "$STATE_DIR" && chmod 0711 -- "$STATE_DIR"
}

# The allow-list is config's, never a list in this script: the serial must
# belong to a role = "mirror" target and to no other kind of target.
require_attachable_serial() {
    local label s serials allowed=false
    for label in "${TARGET_LABELS[@]}"; do
        read -ra serials <<<"${T_SERIALS[$label]}"
        for s in "${serials[@]}"; do
            if [[ "$s" != "$1" ]]; then
                continue
            fi
            if [[ "${T_ROLE[$label]}" == mirror ]]; then
                allowed=true
            else
                refuse "serial $1 belongs to '$label', a role = \"${T_ROLE[$label]}\" target -- a drive of the primary backup is never lent to the VM"
            fi
        done
    done
    if [[ "$allowed" != true ]]; then
        refuse "serial $1 is no role = \"mirror\" target's"
    fi
}

resolve_test_loop() {
    local kind out backing_file backing
    if [[ "$LABEL_EXPLICIT" != true ]]; then
        refuse "DAS_RECOVERY_VM_TEST_LOOP needs the target's full label, not a shorthand"
    fi
    kind="$(stat -L -c '%F:%t' -- "$TEST_LOOP" 2>&1)" || refuse "DAS_RECOVERY_VM_TEST_LOOP=$TEST_LOOP: $kind"
    if [[ "$kind" != "block special file:7" ]]; then
        refuse "DAS_RECOVERY_VM_TEST_LOOP=$TEST_LOOP is not a loop device (stat: $kind) -- the test hatch takes a loop device only"
    fi
    check_path_chars "$TEST_LOOP"
    DISK=$TEST_LOOP
    DISK_DEV="$(readlink -f -- "$DISK")" || refuse "cannot resolve $DISK"
    out="$(lsblk -dnr -o TYPE -- "$DISK_DEV" 2>&1)" || refuse "lsblk cannot read $DISK_DEV: $out"
    if [[ "$out" != loop ]]; then
        refuse "$DISK_DEV is a '$out', not a whole loop device"
    fi
    # A loop over a real drive would be that drive under another name.
    backing_file="$SYS_BLOCK/$(basename -- "$DISK_DEV")/loop/backing_file"
    backing="$(head -n 1 -- "$backing_file" 2>/dev/null)" || backing=""
    if [[ -z "$backing" ]]; then
        refuse "$DISK_DEV has no backing file ($backing_file) -- the test hatch takes a loop over a regular file only"
    fi
    if [[ ! -f "$backing" ]]; then
        refuse "$DISK_DEV is backed by $backing, not a regular file -- the test hatch takes a loop over a regular file only"
    fi
    warn "TEST HATCH (DAS_RECOVERY_VM_TEST_LOOP): lending $DISK instead of $LABEL's drive"
}

# Sets DISK (the by-id path) and DISK_DEV, after checking the disk is the
# one config names: by its serial, read back from the disk itself.
resolve_disk() {
    local serials=() matches=() out type serial
    if [[ -n "$TEST_LOOP" ]]; then
        resolve_test_loop
        return 0
    fi
    read -ra serials <<<"${T_SERIALS[$LABEL]}"
    if ((${#serials[@]} != 1)); then
        refuse "'$LABEL' lists ${#serials[@]} drive serials (${serials[*]:-none}) in $DAS_CONFIG -- a recovery drive is one disk"
    fi
    SERIAL=${serials[0]}
    if [[ ! "$SERIAL" =~ ^[${ASCII_LETTERS}[:digit:]._-]+$ ]]; then
        refuse "the serial '$SERIAL' of '$LABEL' has characters a drive serial does not"
    fi
    require_attachable_serial "$SERIAL"
    shopt -s nullglob
    matches=("$BY_ID"/ata-*_"$SERIAL")
    shopt -u nullglob
    case ${#matches[@]} in
        0) refuse "no disk with serial $SERIAL is attached: $BY_ID/ata-*_$SERIAL matches nothing" ;;
        1) DISK=${matches[0]} ;;
        *) refuse "${#matches[@]} disks match $BY_ID/ata-*_$SERIAL (${matches[*]}) -- expected exactly one" ;;
    esac
    check_path_chars "$DISK"
    DISK_DEV="$(readlink -f -- "$DISK")" || refuse "cannot resolve $DISK"
    out="$(lsblk -dnr -o TYPE,SERIAL -- "$DISK_DEV" 2>&1)" || refuse "lsblk cannot read $DISK_DEV: $out"
    read -r type serial <<<"$out"
    if [[ "$type" != disk ]]; then
        refuse "$DISK resolves to $DISK_DEV, a '$type', not a whole disk"
    fi
    if [[ "$serial" != "$SERIAL" ]]; then
        refuse "$DISK_DEV reports serial '${serial:-none}', not $SERIAL -- the by-id link does not lead to $LABEL's drive"
    fi
    check_filesystem_identity
}

# Partition 2 must carry the filesystem config mounts this target by: a
# serial can be copied into the wrong entry by hand, a filesystem UUID read
# off the disk cannot. The boot record was held to the same mount_uuid
# (check_boot_record), so it is then a record of this disk.
check_filesystem_identity() {
    local want=${T_UUIDS[$LABEL]} part part_dev got
    if [[ -z "$want" ]]; then
        refuse "'$LABEL' has no mount_uuid in $DAS_CONFIG, so the filesystem on its drive cannot be checked -- add it (sudo btrdasd setup --check prints the line to add)"
    fi
    part="$(partition_path "$DISK" 2)"
    part_dev="$(readlink -f -- "$part")" || refuse "cannot resolve $part"
    got="$(lsblk -nr -o UUID -- "$part_dev" 2>&1)" || refuse "lsblk cannot read $part_dev: $got"
    if [[ "$got" != "$want" ]]; then
        refuse "partition 2 of $DISK ($part_dev) carries filesystem ${got:-none}, not $want, the mount_uuid of '$LABEL' -- this is not $LABEL's drive"
    fi
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
# The target units that are not inactive or failed, "unit is state" joined by
# ", " (empty: none). 1, with systemctl's text on stdout, when its answer does
# not have one line per unit.
busy_units() {
    local out states=() i busy=""
    # is-active exits non-zero whenever a unit is not active; the answer is
    # in its output, one line per unit, which is read strictly instead.
    out="$(systemctl is-active "${TARGET_UNITS[@]}" 2>&1)" || :
    mapfile -t states <<<"$out"
    if ((${#states[@]} != ${#TARGET_UNITS[@]})); then
        printf '%s\n' "$out"
        return 1
    fi
    for i in "${!TARGET_UNITS[@]}"; do
        case "${states[$i]}" in
            inactive | failed) ;;
            *) busy+="${busy:+, }${TARGET_UNITS[$i]} is ${states[$i]}" ;;
        esac
    done
    printf '%s\n' "$busy"
}

# --wait-lock: wait for the target units to be inactive and the maintenance
# lock to be free, up to WAIT_LOCK_MIN minutes, looking every POLL_SECS.
# Only looks: the lock is taken by take_lock, non-blocking, as always, so a
# job that slips in between is still refused there, never raced.
wait_for_lock() {
    local deadline busy holder
    [[ -n "$WAIT_LOCK_MIN" ]] || return 0
    deadline=$((SECONDS + WAIT_LOCK_MIN * MINUTE_SECS))
    while :; do
        busy="$(busy_units)" || refuse "cannot tell whether ${TARGET_UNITS[*]} are running (systemctl said: $busy)"
        if [[ -z "$busy" ]]; then
            # A lock file that does not exist yet is free: take_lock creates it.
            if [[ ! -e "$MAINTENANCE_LOCK" ]] || (flock -n 9) 9<"$MAINTENANCE_LOCK"; then
                return 0
            fi
            holder="$(head -n 1 -- "$MAINTENANCE_LOCK" 2>/dev/null)" || holder=""
            busy="the DAS maintenance lock (held by: ${holder:-(no holder line)})"
        fi
        if ((SECONDS >= deadline)); then
            refuse "waited $WAIT_LOCK_MIN min for $busy -- nothing held; try again, or let it finish"
        fi
        log "waiting for $busy"
        sleep "$POLL_SECS"
    done
}

check_units() {
    local busy
    busy="$(busy_units)" || refuse "cannot tell whether ${TARGET_UNITS[*]} are running (systemctl said: $busy)"
    if [[ -n "$busy" ]]; then
        refuse "$busy -- wait until it has finished"
    fi
}

check_not_mounted() {
    local mounted
    mounted="$(mounted_partitions "$DISK_DEV")" || refuse "cannot list the mounts of $DISK_DEV"
    if [[ -n "$mounted" ]]; then
        refuse "$DISK_DEV is mounted on this host: $mounted -- unmount it first"
    fi
}

# A managed-save image is the memory of a recovery OS that was RUNNING when
# it was saved (virsh managedsave, virt-manager's Save, libvirt-guests at a
# host shutdown); the domain then reads "shut off", and a start resumes that
# OS where it stopped instead of booting it -- and a domain started paused
# would then not be one that never ran. Refused, with what it is; this
# script never discards it: that is the operator's decision. So is a domain
# whose image cannot be told (dominfo says "unknown", or nothing).
check_no_managed_save() {
    local out line saved=""
    out="$(virsh_ dominfo "$DOMAIN" 2>&1)" || refuse "cannot read $DOMAIN's information (whether it has a managed-save image): $out"
    while IFS= read -r line; do
        if [[ "$line" =~ ^Managed\ save:[[:space:]]+([^[:space:]].*)$ ]]; then
            saved=${BASH_REMATCH[1]}
        fi
    done <<<"$out"
    case "$saved" in
        no) return 0 ;;
        yes) refuse "$DOMAIN has a managed-save image: the saved memory of a recovery OS that was RUNNING when it was saved (virsh managedsave, virt-manager's Save, or libvirt-guests at a host shutdown). A start would resume that OS where it stopped -- its disk included, against a partition 2 this host may have written since -- instead of booting it afresh. This script never discards it: the decision is yours. To throw that state away (whatever that OS had not written to its disk is lost): virsh --connect $LIBVIRT_URI managedsave-remove $DOMAIN -- then run the session again" ;;
        *) refuse "cannot tell whether $DOMAIN has a managed-save image (virsh dominfo says '$(printable "${saved:-nothing of it}")') -- a session never starts a domain that may resume a saved OS" ;;
    esac
}

check_domain_idle() {
    local state xml sources
    state="$(virsh_ domstate "$DOMAIN" 2>&1)" || refuse "cannot read the state of $DOMAIN: $state -- is it defined? $SELF define"
    if [[ "$state" != "shut off" ]]; then
        refuse "$DOMAIN is $state -- it must be shut off"
    fi
    check_no_managed_save
    xml="$(virsh_ dumpxml --inactive "$DOMAIN" 2>&1)" || refuse "cannot read $DOMAIN's definition: $xml"
    sources="$(disk_sources "$xml")"
    if [[ -n "$sources" ]]; then
        refuse "$DOMAIN already has a disk attached (${sources//$'\n'/, }) -- a session was not ended: $SELF session-end <label>"
    fi
}

check_no_live_holder() {
    local f
    shopt -s nullglob
    for f in "$STATE_DIR"/*.holder; do
        # In a two-drive run the other drive's holder is the run's own.
        if [[ -n "${DAS_RECOVERY_VM_LOCK_FD:-}" && "$f" != "$HOLDER_FILE" ]]; then
            continue
        fi
        if ! read_record "$f"; then
            refuse "unreadable holder record $f -- check '$SELF status', then remove it"
        fi
        if holder_alive "$REC_PID" "$REC_DEV"; then
            refuse "a previous session's disk holder is still running (pid $REC_PID, holding $REC_DEV) -- finish it: $SELF session-end $(basename -- "$f" .holder)"
        fi
        warn "removing the stale record $f (pid $REC_PID no longer holds $REC_DEV)"
        rm -f -- "$f"
    done
    shopt -u nullglob
}

# ---------------------------------------------------------------------------
# The boot record (bd DAS-Backup-Manager-1yg)
# ---------------------------------------------------------------------------
age_text() {
    local s=$1
    if ((s < 0)); then
        printf 'in the future'
    elif ((s >= 86400)); then
        printf '%dd %dh ago' $((s / 86400)) $((s % 86400 / 3600))
    else
        printf '%dh %02dm ago' $((s / 3600)) $((s % 3600 / 60))
    fi
}

utc() {
    date -u -d "@$1" '+%Y-%m-%d %H:%M UTC'
}

# This label's last session time from SESSIONS_FILE into LAST_SESSION (empty
# when none is recorded). 1, with SESSIONS_FAILURE set, when that cannot be
# told: not a regular file, unreadable, or no time on this label's line.
last_session_time() {
    local lines label t
    LAST_SESSION=""
    SESSIONS_FAILURE=""
    if [[ ! -e "$SESSIONS_FILE" && ! -L "$SESSIONS_FILE" ]]; then
        return 0
    fi
    if [[ -L "$SESSIONS_FILE" || ! -f "$SESSIONS_FILE" ]]; then
        SESSIONS_FAILURE="$SESSIONS_FILE is not a regular file"
        return 1
    fi
    if ! lines="$(cat -- "$SESSIONS_FILE" 2>&1)"; then
        SESSIONS_FAILURE="cannot read $SESSIONS_FILE: $lines"
        return 1
    fi
    # Never empty when this script wrote it: an empty one was cut short.
    if [[ -z "$lines" ]]; then
        SESSIONS_FAILURE="$SESSIONS_FILE is empty"
        return 1
    fi
    local n=0 line
    while IFS= read -r line; do
        n=$((n + 1))
        if [[ ! "$line" =~ ^([^[:space:]]+)\ ([123456789][[:digit:]]{0,17})$ ]]; then
            SESSIONS_FAILURE="line $n of $SESSIONS_FILE is not '<label> <seconds>': '$line'"
            return 1
        fi
        label=${BASH_REMATCH[1]}
        t=${BASH_REMATCH[2]}
        if [[ "$label" == "$LABEL" ]]; then
            LAST_SESSION=$t
        fi
    done <<<"$lines"
}

is_target_label() {
    local l
    for l in "${TARGET_LABELS[@]}"; do
        [[ "$l" != "$1" ]] || return 0
    done
    return 1
}

# Record $1 (seconds since the epoch) as this label's session time: one line
# per label, the file replaced whole, readable by all and written by its
# owner (root) only. Only well-formed lines go back, and none silently:
# another target's line whose time cannot be read is written back at $1, so
# that drive's next session waits for a new record too; any other bad line
# is dropped. 1, with SESSIONS_FAILURE set, when it cannot be written.
record_session_time() {
    local lines="" line label tmp out="" n=0
    SESSIONS_FAILURE=""
    if [[ -L "$SESSIONS_FILE" ]] || [[ -e "$SESSIONS_FILE" && ! -f "$SESSIONS_FILE" ]]; then
        SESSIONS_FAILURE="$SESSIONS_FILE is not a regular file"
        return 1
    fi
    if [[ -e "$SESSIONS_FILE" ]] && ! lines="$(cat -- "$SESSIONS_FILE" 2>&1)"; then
        SESSIONS_FAILURE="cannot read $SESSIONS_FILE: $lines"
        return 1
    fi
    if [[ -n "$lines" ]]; then
        while IFS= read -r line; do
            n=$((n + 1))
            label=${line%%[[:space:]]*}
            if [[ "$line" =~ ^([^[:space:]]+)\ ([123456789][[:digit:]]{0,17})$ ]]; then
                [[ "${BASH_REMATCH[1]}" == "$LABEL" ]] || out+="$line"$'\n'
            elif [[ "$label" == "$LABEL" ]]; then
                : # this drive's own line: replaced below
            elif [[ -n "$label" ]] && is_target_label "$label"; then
                out+="$label $1"$'\n'
                warn "rewrote line $n of $SESSIONS_FILE ('$line') as '$label $1': its time could not be read, so that drive's next session waits for a new boot record"
            else
                warn "dropped line $n of $SESSIONS_FILE ('$line'): not '<label> <seconds>' for any target"
            fi
        done <<<"$lines"
    fi
    out+="$LABEL $1"$'\n'
    tmp="$SESSIONS_FILE.new.$$"
    if ! printf '%s' "$out" >"$tmp"; then
        rm -f -- "$tmp"
        SESSIONS_FAILURE="cannot write $tmp"
        return 1
    fi
    # On disk before it replaces the old one, and the rename on disk too: the
    # start of a session must survive the host crashing during it.
    if ! chmod 0644 -- "$tmp" || ! sync -- "$tmp" || ! mv -f -- "$tmp" "$SESSIONS_FILE"; then
        rm -f -- "$tmp"
        SESSIONS_FAILURE="cannot put $tmp in place as $SESSIONS_FILE"
        return 1
    fi
    if ! sync -- "$(dirname -- "$SESSIONS_FILE")"; then
        SESSIONS_FAILURE="cannot flush the directory of $SESSIONS_FILE"
        return 1
    fi
}

# Which boot record, and where the session times beside it are kept. The test
# hatch and DAS_RECOVERY_OS_STATE come together or not at all: the record is
# what keeps a real drive's backups safe from its own OS, and a session on the
# hatch's loop file must never read, or write the session times of, the real
# drive its label names.
resolve_os_state() {
    if [[ -n "${DAS_RECOVERY_OS_STATE:-}" && -z "$TEST_LOOP" ]]; then
        refuse "DAS_RECOVERY_OS_STATE is set: it points the boot-record check at another file, and is honoured only with the test hatch DAS_RECOVERY_VM_TEST_LOOP, which lends a loop file -- the record is what keeps a real drive's backups safe from its own OS"
    fi
    if [[ -n "$TEST_LOOP" && -z "${DAS_RECOVERY_OS_STATE:-}" ]]; then
        refuse "the test hatch needs DAS_RECOVERY_OS_STATE: a session on a loop file must not read, or write the session times of, the record of the real drive its label names"
    fi
    if [[ -n "${DAS_RECOVERY_OS_STATE:-}" ]]; then
        OS_STATE_FILE=$DAS_RECOVERY_OS_STATE
        warn "TEST: the boot record is read from $OS_STATE_FILE (DAS_RECOVERY_OS_STATE)"
    fi
    SESSIONS_FILE="$(dirname -- "$OS_STATE_FILE")/recovery-os-vm-sessions"
    HISTORY_FILE="$(dirname -- "$OS_STATE_FILE")/recovery-os-vm-history.jsonl"
}

# Read this drive's boot record and go on only when it does not say btrbk
# will run when its OS boots -- before anything is taken. "may" goes on: the
# session guard is what keeps btrbk from running, and what the record names
# is masked by it. The record's own facts are shown whatever they say.
check_boot_record() {
    local file out rc=0 key value schemas=0 schema="" schematype="" entry="" checked="" checkedtype="" error=""
    local verdict="" units_state="" when now age p problems=() hints=() reasons=() units=() runners=() hint=""
    local fs="" fstype="" want
    resolve_os_state
    file=$OS_STATE_FILE
    if [[ ! -e "$file" ]]; then
        refuse "no boot record: $file does not exist. The nightly backup run writes it when it checks the recovery OSes -- let one run with this drive attached, then start again"
    fi
    # stderr first: a file that cannot be opened fails the < redirection.
    out="$(jq -r --arg label "$LABEL" "$BOOT_RECORD_JQ" 2>&1 <"$file")" || rc=$?
    if ((rc != 0)); then
        refuse "the boot record $file cannot be read: ${out:-jq exit $rc}"
    fi
    while IFS=$'\t' read -r key value; do
        case "$key" in
            schema)
                schemas=$((schemas + 1))
                schema=$value
                ;;
            schematype) schematype=$value ;;
            entry) entry=$value ;;
            checked) checked=$value ;;
            checkedtype) checkedtype=$value ;;
            fs) fs=$value ;;
            fstype) fstype=$value ;;
            error) error=$value ;;
            verdict) verdict=$value ;;
            reason) reasons+=("$value") ;;
            runner) runners+=("$value") ;;
            units) units_state=$value ;;
            unit) units+=("$value") ;;
            agent) REC_AGENT=$value ;;
            agentwhy) REC_AGENT_WHY=$value ;;
        esac
    done <<<"$out"
    if ((schemas != 1)); then
        refuse "the boot record $file is not one JSON document ($schemas found)"
    fi
    if [[ "$schematype" != number ]]; then
        refuse "the boot record $file is not one this script can read: its schema_version is not a number ('$schema', a $schematype)"
    fi
    if [[ " $OS_STATE_SCHEMAS " != *" $schema "* ]]; then
        refuse "the boot record $file is schema $schema -- this script reads schemas ${OS_STATE_SCHEMAS// / and } only (an older record has no btrbk-at-boot verdict); the next backup run writes it again"
    fi
    if [[ "$entry" != present ]]; then
        refuse "the boot record $file has no entry for '$LABEL' -- the nightly backup run writes one when it checks this drive; let one run with it attached"
    fi
    # A JSON number of whole seconds above 0: a string -- "015260430204" --
    # would reach bash arithmetic as octal, and 0 is no time at all.
    if [[ "$checkedtype" != number || ! "$checked" =~ ^[123456789][[:digit:]]{0,17}$ ]]; then
        refuse "the boot record for '$LABEL' has no check time ('$checked', a $checkedtype) -- a whole number of seconds above 0 is needed"
    fi
    when="$(utc "$checked" 2>&1)" ||
        refuse "the boot record for '$LABEL' has a check time this host cannot read as a date ($checked: $when) -- not a time in seconds"
    now=$(date +%s)
    age=$((now - checked))
    # Minutes ahead is a clock out of step (below, and overridable); more than
    # a day ahead is a record that is wrong -- a time in milliseconds, say.
    if ((checked > now + 86400)); then
        refuse "the boot record for '$LABEL' is dated more than a day in the future ($when) -- not a clock out of step but a wrong record (a time in milliseconds?)"
    fi
    if [[ -n "$error" ]]; then
        refuse "the boot record for '$LABEL' (checked $when, $(age_text "$age")) has no inspected OS ($error)"
    fi
    if [[ "$verdict" != no && "$verdict" != will && "$verdict" != may ]]; then
        refuse "the boot record for '$LABEL' (checked $when, $(age_text "$age")) has no btrbk-at-boot verdict ('$verdict')"
    fi
    # The record must be of the filesystem config mounts this drive by (bd
    # DAS-Backup-Manager-df0): a label pointed at another drive, or a
    # filesystem made again, must never inherit the old one's verdict. Never
    # overridable. Partition 2 is held to config's mount_uuid when the disk
    # is resolved (check_filesystem_identity), so a record that passes here is
    # of the disk that is lent -- the test hatch's loop file excepted.
    want=${T_UUIDS[$LABEL]}
    if [[ -z "$want" ]]; then
        refuse "'$LABEL' has no mount_uuid in $DAS_CONFIG, so neither its drive nor its boot record can be tied to a filesystem -- add it (sudo btrdasd setup --check prints the line to add)"
    fi
    if [[ "$fstype" != string || -z "$fs" || "$fs" == unknown ]]; then
        refuse "the boot record for '$LABEL' (checked $when, $(age_text "$age")) does not name the filesystem it was read from (a btrdasd older than this script wrote it, or the filesystem could not be told) -- a record not tied to the drive's filesystem is never trusted, whatever the options: let the next backup run, with this drive attached, record it again"
    fi
    if [[ "$fs" != "$want" ]]; then
        refuse "the boot record for '$LABEL' (checked $when, $(age_text "$age")) was read from filesystem $(printable "$fs"), not $want, the mount_uuid of '$LABEL' in $DAS_CONFIG -- the label names another drive now, or its filesystem was made again since; never overridable: let the next backup run, with this drive attached, record it again"
    fi
    log "boot record for $LABEL ($file, schema $schema):"
    log "  checked        $when, $(age_text "$age")"
    log "  filesystem     $fs (the mount_uuid of $LABEL)"
    if [[ "$units_state" == listed ]]; then
        log "  enabled units  $(IFS=,; p="${units[*]}"; printf '%s' "${p//,/, }") (${#units[@]})"
    else
        log "  enabled units  $units_state"
    fi
    log "  btrbk at boot  $verdict"
    for p in "${reasons[@]}"; do
        log "    - $p"
    done
    log "  guest agent    ${REC_AGENT:-not recorded}${REC_AGENT_WHY:+ ($REC_AGENT_WHY)}"
    VERDICT=$verdict
    REC_REASONS=("${reasons[@]}")
    REC_RUNNERS=("${runners[@]}")
    REC_UNITS=()
    if [[ "$units_state" == listed ]]; then
        REC_UNITS=("${units[@]}")
    fi

    if [[ "$verdict" == may ]]; then
        log "boot record: btrbk may run when this OS boots: the session guard is what keeps it from running"
    fi
    # Unattended (decisions 1 and 3): only through an agent the record says
    # runs at boot, and never on "will" -- that is for an operator at the
    # console. Neither is for --accept-boot-record-risk to let through.
    if [[ "$UNATTENDED" == true ]]; then
        if [[ "$verdict" == will ]]; then
            refuse "the boot record says btrbk will run when this OS boots: a \"will\" drive is updated attended only, never --unattended (its operator disables that in the session) -- whatever the options"
        fi
        if [[ "$REC_AGENT" != "read true true" ]]; then
            refuse "an unattended session needs the recovery OS's QEMU guest agent installed and started at boot, and its boot record says: ${REC_AGENT:-nothing}${REC_AGENT_WHY:+ ($REC_AGENT_WHY)} -- run one attended session and install it there (pacman -S qemu-guest-agent), then let a backup run record the drive again"
        fi
    fi
    # Attended, a "will" drive goes on (decision 3 of bd 8249, 2026-10-04):
    # the guard masks what the record names and binds a refusing btrbk, and
    # the operator at the console disables it for good. Said before the boot
    # and again once the guard confirms. A "will" whose guard does not
    # confirm is shut down (exit 6), as a "may" is.
    # (--unattended on "will" was refused above.)
    if [[ "$verdict" == will ]]; then
        WILL_BANNER=true
        warn "$WILL_BANNER_TEXT"
    fi
    if ((checked > now + 300)); then
        problems+=("the record is dated in the future ($when): the clock of the run that wrote it, or this one, is wrong")
        hints+=("let the next backup run, with this drive attached, record it again")
    elif ((age > MAX_RECORD_AGE_DAYS * 86400)); then
        problems+=("the record is $(age_text "$age" | sed 's/ ago$//') old, older than $MAX_RECORD_AGE_DAYS days: it no longer describes the drive")
        hints+=("let the next backup run, with this drive attached, record it again")
    fi
    if ! last_session_time; then
        problems+=("cannot tell when this drive's last VM session ended ($SESSIONS_FAILURE)")
        hints+=("remove the bad line, or the whole file, at $SESSIONS_FILE, then run a backup with the drive attached")
    elif [[ -n "$LAST_SESSION" ]] && ((checked <= LAST_SESSION)); then
        problems+=("the record was made before this drive's last VM session ended ($(utc "$LAST_SESSION")): the OS may have changed in it")
        hints+=("let the next backup run, with this drive attached, record it again")
    fi
    if ((${#problems[@]} == 0)); then
        if [[ "$verdict" == no ]]; then
            log "boot record: btrbk will not run when this OS boots"
        fi
        return 0
    fi
    if [[ "$ACCEPT_BOOT_RISK" == true ]]; then
        for p in "${problems[@]}"; do
            warn "OVERRIDDEN (--accept-boot-record-risk): $p"
            BOOT_OVERRIDES+=("$p")
        done
        return 0
    fi
    # Each piece of advice once, in the order the problems were found.
    for p in "${hints[@]}"; do
        [[ "; $hint; " == *"; $p; "* ]] || hint+="${hint:+; }$p"
    done
    refuse "$(IFS=';'; printf '%s' "${problems[*]}" | sed 's/;/; /g') (the record was checked $when, $(age_text "$age")) -- $hint"
}

# ---------------------------------------------------------------------------
# The session guard (bd DAS-Backup-Manager-0zm)
# ---------------------------------------------------------------------------

# A string from the recovery OS (a record, or its report) as it may be shown:
# printable characters only, and not without end.
printable() {
    local s=${1//[^[:print:]]/?}
    printf '%s' "${s:0:300}"
}

# Words joined as a list is read: "a, b and c".
join_and() {
    local out="" i
    for ((i = 1; i <= $#; i++)); do
        if ((i == 1)); then
            out=${!i}
        elif ((i == $#)); then
            out+=" and ${!i}"
        else
            out+=", ${!i}"
        fi
    done
    printf '%s' "$out"
}

# 1 boot, 2 boots: $1 the count, $2 the word.
count_of() {
    if (($1 == 1)); then printf '1 %s' "$2"; else printf '%s %ss' "$1" "$2"; fi
}

# Whether $1 is a name this script puts into a credential: the ASCII
# characters unit names are made of -- no ":", which systemd refuses in a
# credential's name -- a unit that can run something, never one of the
# guard's own, and no template (name@.service): `systemctl show` refuses a
# template's name, so the report could never vouch for one; an instance is
# named in full.
guard_maskable() {
    [[ ${#1} -le 200 && "$1" =~ $UNIT_NAME_RE && "$1" != das-vm-guard* && ! "$1" =~ @\.[${ASCII_LOWER}]+$ ]]
}

# $1 a name the boot record gives ($2 says where): into MASKS once, or, when
# it cannot be masked, said and kept in UNMASKABLE.
guard_candidate() {
    local m shown
    if ! guard_maskable "$1"; then
        shown="'$(printable "$1")'"
        warn "session guard: not masked: $shown ($2): only a service, timer, socket or path unit of a plain ASCII name -- no ':', no template -- is ever masked, and never the guard's own"
        for m in "${UNMASKABLE[@]}"; do
            [[ "$m" != "$shown" ]] || return 0
        done
        UNMASKABLE+=("$shown")
        return 0
    fi
    for m in "${MASKS[@]}"; do
        [[ "$m" != "$1" ]] || return 0
    done
    MASKS+=("$1")
}

# What the guard masks: btrbk's units and the cron daemons always; every
# cron daemon the record lists among the enabled units; every runner it found
# (the unit, or the cron daemon running a cron file); and the unit each
# reason begins with ("unit may run btrbk: ...", "timer starts unit, which
# ..."). The defaults first, the rest in order, at most GUARD_MAX_MASKS; a
# name past that, or one that cannot be masked, is in UNMASKABLE. Each mask is
# two strings: an empty unit, and a drop-in sorting last whose condition can
# never hold (a drop-in of the OS's own can make the empty unit whole again,
# but not undo a condition applied after it). Then the strings that carry it
# all, and the lines its report must be.
build_guard() {
    local u r w extra=() sorted=() over=() unit_b64 report_b64 dropin_b64 stub_b64 never_b64 list digest
    MASKS=("${GUARD_DEFAULT_MASKS[@]}")
    UNMASKABLE=()
    for u in "${REC_UNITS[@]}"; do
        [[ "$u" != *cron* ]] || guard_candidate "$u" "a cron daemon the boot record lists as enabled"
    done
    for r in "${REC_RUNNERS[@]}"; do
        [[ -z "$r" ]] || guard_candidate "$r" "the boot record found it running btrbk"
    done
    for r in "${REC_REASONS[@]}"; do
        [[ "$r" =~ $REASON_UNITS_RE ]] || continue
        for w in "${BASH_REMATCH[1]}" "${BASH_REMATCH[3]}"; do
            w=${w%[:,]}
            if [[ "$w" =~ \.(service|timer|socket|path)$ ]]; then
                guard_candidate "$w" "a reason in the boot record begins with it"
            fi
        done
    done
    extra=("${MASKS[@]:${#GUARD_DEFAULT_MASKS[@]}}")
    if ((${#extra[@]} > 0)); then
        mapfile -t sorted < <(printf '%s\n' "${extra[@]}" | sort)
        u=$((GUARD_MAX_MASKS - ${#GUARD_DEFAULT_MASKS[@]}))
        if ((${#sorted[@]} > u)); then
            over=("${sorted[@]:$u}")
            sorted=("${sorted[@]:0:$u}")
            warn "session guard: not masked: ${over[*]} -- the guard masks at most $GUARD_MAX_MASKS units"
            UNMASKABLE+=("${over[*]} (it masks at most $GUARD_MAX_MASKS)")
        fi
    fi
    MASKS=("${GUARD_DEFAULT_MASKS[@]}" "${sorted[@]}")
    list="$(IFS=,; printf '%s' "${MASKS[*]}")"
    # A heartbeat names the masks by a digest of their list: the first 16
    # hex digits of its SHA-256. Computed here and given to the reporter, so
    # the recovery OS needs no tool for it.
    digest="$(printf '%s' "$list" | sha256sum)" || refuse "cannot compute the digest of the session guard's masks"
    digest=${digest:0:16}
    [[ "$digest" =~ ^[0123456789abcdef]{16}$ ]] || refuse "cannot compute the digest of the session guard's masks"
    GUARD_EXPECT="das-vm-guard engaged ${#MASKS[@]} masks $list"
    GUARD_EXPECT_LIFTED="das-vm-guard lifted ${#MASKS[@]} masks $list"
    GUARD_EXPECT_C="das-vm-guard engaged ${#MASKS[@]} masks sha256:$digest"
    GUARD_EXPECT_LIFTED_C="das-vm-guard lifted ${#MASKS[@]} masks sha256:$digest"
    unit_b64="$(guard_unit_text | base64 -w0)" || refuse "cannot encode the session guard's unit"
    report_b64="$(guard_report_text | base64 -w0)" || refuse "cannot encode the session guard's report unit"
    dropin_b64="$(printf '[Unit]\nWants=%s %s\n' "$GUARD_UNIT" "$GUARD_REPORT_UNIT" | base64 -w0)" ||
        refuse "cannot encode the session guard's drop-in"
    stub_b64="$(guard_stub_text | base64 -w0)" || refuse "cannot encode the refusing btrbk"
    never_b64="$(printf '[Unit]\nConditionPathExists=%s\n' "$GUARD_NEVER" | base64 -w0)" ||
        refuse "cannot encode the masks' condition"
    GUARD_ENTRIES=(
        "io.systemd.credential.binary:systemd.extra-unit.$GUARD_UNIT=$unit_b64"
        "io.systemd.credential.binary:systemd.extra-unit.$GUARD_REPORT_UNIT=$report_b64"
        "io.systemd.credential.binary:systemd.unit-dropin.$GUARD_DROPIN=$dropin_b64"
        "io.systemd.credential.binary:$GUARD_STUB_CRED=$stub_b64"
    )
    for u in "${MASKS[@]}"; do
        GUARD_ENTRIES+=("io.systemd.credential:systemd.extra-unit.$u=")
        GUARD_ENTRIES+=("io.systemd.credential.binary:systemd.unit-dropin.$u~$GUARD_MASK_DROPIN=$never_b64")
    done
    log "session guard: a refusing btrbk bound over $(join_and "${GUARD_BTRBK_PATHS[@]}") where present; masks $(IFS=,; p="${MASKS[*]}"; printf '%s' "${p//,/, }")"
}

# What the recovery OS reads at its console and above every login prompt
# while the guard holds (agetty >= 2.41 reads /run/issue.d always): the
# guide's order, condensed. No $, %, quotes or backslashes: it goes through
# systemd's command-line parsing and sh. A guard that failed halfway has no
# stop to run, so step 1 names each place it binds, exactly (an umount of
# one not mounted only says so).
guard_message_lines() {
    printf '%s\n' \
        "das-vm-guard: btrbk cannot run in this VM session (DAS recovery OS updater)." \
        "  To update, follow the first update in the disaster recovery guide. In short:" \
        "  1. lift it:    systemctl stop das-vm-guard   (if it failed: umount ${GUARD_BTRBK_PATHS[*]})" \
        "  2. keyrings:   pacman -Sy archlinux-keyring cachyos-keyring" \
        "  3. drivers:    pin both worlds into the initramfs first (the guide, step 2)" \
        "  4. upgrade:    pacman -Su" \
        "  5. check:      pacman -Qkk btrbk   must find 0 altered files" \
        "  6. re-engage:  systemctl start das-vm-guard   (lifted, pacman hooks and units not masked can run btrbk)" \
        "  7. reboot, check uname -r, then systemctl poweroff"
}

# What runs in place of btrbk while the guard holds: it refuses, and says so
# on the kernel log -- the evidence that something tried. A credential of its
# own, so no line of it passes through systemd's parsing. 2>/dev/null comes
# before > /dev/kmsg: redirections apply left to right, and a caller who
# cannot write the kernel log (not root) would otherwise see that failure
# instead of only the refusal (bd DAS-Backup-Manager-uo39).
guard_stub_text() {
    cat <<'EOF'
#!/bin/sh
# das-vm-guard (DAS recovery OS updater): btrbk cannot run in this VM session.
echo "das-vm-guard: refused: btrbk $* (pid $$, parent $PPID $(cat /proc/$PPID/comm 2>/dev/null))" 2>/dev/null > /dev/kmsg
echo "das-vm-guard: btrbk cannot run in this VM session -- lift the guard first: systemctl stop das-vm-guard" >&2
exit 1
EOF
}

# The guard: the refusing btrbk bound over each btrbk present -- after the
# root is remounted, before udev's coldplug (whose rules can run programs),
# local-fs-pre.target and sysinit.target, so before every unit with default
# dependencies. Not before: systemd's own earliest units, and anything
# ordered before systemd-remount-fs.service. A path that already leads to the
# refusing btrbk (a symlink to a bound one) is not bound twice. Lifted again by
# `systemctl stop das-vm-guard`. Skipped in the initrd, which loads extra
# units too. $$ is a literal $ to systemd; nothing in it may contain %.
guard_unit_text() {
    local p echoes="" line paths
    paths="${GUARD_BTRBK_PATHS[*]}"
    while IFS= read -r line; do
        echoes+="echo \"$line\"; "
    done < <(guard_message_lines)
    cat <<EOF
[Unit]
Description=DAS VM session guard: btrbk cannot run in this VM session
DefaultDependencies=no
ConditionPathExists=!/etc/initrd-release
After=systemd-remount-fs.service
Before=systemd-udev-trigger.service local-fs-pre.target sysinit.target

[Service]
Type=oneshot
RemainAfterExit=yes
TimeoutStartSec=60
ImportCredential=$GUARD_STUB_CRED
ExecStart=/usr/bin/sh -c 'install -D -m 0755 "\$\$CREDENTIALS_DIRECTORY/$GUARD_STUB_CRED" $GUARD_STUB'
ExecStart=/usr/bin/sh -c 'for p in $paths; do if [ -e "\$\$p" ] && ! [ "\$\$p" -ef $GUARD_STUB ]; then /usr/bin/mount --bind $GUARD_STUB "\$\$p" || exit 1; fi; done'
ExecStart=-/usr/bin/sh -c 'echo "das-vm-guard: btrbk cannot run in this VM session; before pacman: systemctl stop das-vm-guard; after it: pacman -Qkk btrbk must find 0 altered files" > /dev/kmsg'
ExecStart=-/usr/bin/sh -c 'mkdir -p ${GUARD_ISSUE%/*} && { ${echoes}echo; } > $GUARD_ISSUE'
ExecStart=-/usr/bin/timeout 5 /usr/bin/sh -c '{ echo; ${echoes}echo; } > /dev/console'
EOF
    for ((p = ${#GUARD_BTRBK_PATHS[@]} - 1; p >= 0; p--)); do
        printf 'ExecStop=-/usr/bin/umount %s\n' "${GUARD_BTRBK_PATHS[$p]}"
    done
    printf 'ExecStop=-/usr/bin/rm -f %s\n' "$GUARD_ISSUE"
    printf '%s\n' "ExecStop=-/usr/bin/sh -c 'echo \"das-vm-guard: lifted: btrbk can run again; its units and cron stay masked until the VM powers off\" > /dev/kmsg'"
}

# The report: at the start of every boot and then every minute for as long as
# the VM runs, one line through the virtio port with this boot's id and the
# line's number in this boot (from 1) -- the host notices a boot that came
# without the guard by its silence after a reset. A boot's first line needs
# the guard active ("engaged"); once one was sent, an inactive guard was
# stopped for the update ("lifted"), its masks still checked. That "once"
# lives in /run (with the boot's id and the count), so a reporter restarted
# while the guard is lifted (Restart=, a few times an hour) goes on with
# "lifted", never a first-line NOT engaged. A unit masked is one that cannot
# be started: masked outright, or this guard's empty unit (which systemd 262
# loads as "bad-setting": the generator writes an empty credential as one
# newline) or that unit made whole by a drop-in of the OS's own ("loaded")
# -- in both cases only with the guard's never-true condition as the LAST
# drop-in. What cannot be checked this round -- systemd not answering (a
# re-exec during an upgrade), a unit between states, findmnt or readlink
# failing -- sends nothing and looks again in 5 s: never "not covered". A
# line names its masks in full when the reporter starts or its state
# changes, by their digest otherwise. Waits for the port at most two
# minutes; ordered before nothing.
guard_report_text() {
    local script n=${#MASKS[@]} list digest
    list="$(IFS=,; printf '%s' "${MASKS[*]}")"
    digest=${GUARD_EXPECT_C##*sha256:}
    script="port=/dev/virtio-ports/$GUARD_PORT; st=/run/das-vm-guard/report.state; "
    script+="boot=\$\$(cat /proc/sys/kernel/random/boot_id) || exit 1; n=0; "
    script+="while [ ! -e \"\$\$port\" ] && [ \"\$\$n\" -lt 120 ]; do sleep 1; n=\$\$((n + 1)); done; "
    script+="if [ ! -e \"\$\$port\" ]; then echo \"das-vm-guard: no port to the host\"; exit 0; fi; "
    script+="mkdir -p /run/das-vm-guard; seq=0; eng=0; last=; "
    script+="if { read -r b s e < \"\$\$st\"; } 2>/dev/null && [ \"\$\$b\" = \"\$\$boot\" ]; then case \"\$\$s\" in *[!0-9]*) ;; [0-9]*) seq=\$\$s ;; esac; if [ \"\$\$e\" = 1 ]; then eng=1; fi; fi; "
    script+="while :; do bad=; mode=; "
    script+="a=\$\$(systemctl show -P ActiveState $GUARD_UNIT) || { sleep 5; continue; }; "
    script+="case \"\$\$a\" in active) mode=engaged ;; inactive) if [ \"\$\$eng\" = 1 ]; then mode=lifted; else bad=\"\$\$bad $GUARD_UNIT is inactive;\"; fi ;; activating|deactivating|reloading|refreshing) sleep 5; continue ;; *) bad=\"\$\$bad $GUARD_UNIT is \$\$a;\" ;; esac; "
    script+="ok=1; for u in ${MASKS[*]}; do s=\$\$(systemctl show -P LoadState \"\$\$u\") || { ok=0; break; }; "
    script+="case \"\$\$s\" in masked) ;; bad-setting|loaded) f=\$\$(systemctl show -P FragmentPath \"\$\$u\") || { ok=0; break; }; d=\$\$(systemctl show -P DropInPaths \"\$\$u\") || { ok=0; break; }; z=; for x in \$\$d; do z=\$\$x; done; "
    script+="if [ \"\$\$f\" != \"$GUARD_EARLY/\$\$u\" ] || [ \"\$\$z\" != \"$GUARD_EARLY/\$\$u.d/$GUARD_MASK_DROPIN.conf\" ]; then bad=\"\$\$bad \$\$u is not masked (\$\$s);\"; fi ;; "
    script+="*) bad=\"\$\$bad \$\$u is not masked (\$\$s);\" ;; esac; done; "
    script+="if [ \"\$\$ok\" = 1 ] && [ \"\$\$mode\" = engaged ]; then m=\$\$(findmnt -rn -o TARGET) || ok=0; set -f; "
    script+="for p in ${GUARD_BTRBK_PATHS[*]}; do if [ \"\$\$ok\" = 1 ] && [ -e \"\$\$p\" ]; then t=\$\$(readlink -f \"\$\$p\") || { ok=0; break; }; c=; for x in \$\$m; do if [ \"\$\$x\" = \"\$\$t\" ]; then c=1; fi; done; "
    script+="if ! [ \"\$\$p\" -ef $GUARD_STUB ] || [ -z \"\$\$c\" ]; then bad=\"\$\$bad \$\$p is not covered;\"; fi; fi; done; set +f; fi; "
    script+="if [ \"\$\$ok\" = 0 ]; then sleep 5; continue; fi; "
    script+="if [ -n \"\$\$bad\" ]; then body=\"das-vm-guard NOT engaged:\$\$bad\"; elif [ \"\$\$mode\" = \"\$\$last\" ]; then body=\"das-vm-guard \$\$mode $n masks sha256:$digest\"; else body=\"das-vm-guard \$\$mode $n masks $list\"; fi; "
    script+="line=\"\$\$body seq \$\$((seq + 1)) boot \$\$boot\"; echo \"\$\$line\"; "
    script+="if echo \"\$\$line\" >> \"\$\$port\"; then seq=\$\$((seq + 1)); last=; if [ -z \"\$\$bad\" ]; then last=\$\$mode; if [ \"\$\$mode\" = engaged ]; then eng=1; fi; fi; "
    script+="echo \"\$\$boot \$\$seq \$\$eng\" > \"\$\$st.new\" && mv -f \"\$\$st.new\" \"\$\$st\"; fi; "
    script+="sleep $GUARD_HEARTBEAT_SECS; done"
    cat <<EOF
[Unit]
Description=DAS VM session guard: report to the host
ConditionPathExists=!/etc/initrd-release
After=$GUARD_UNIT
StartLimitIntervalSec=1h
StartLimitBurst=4

[Service]
Type=simple
Restart=on-failure
RestartSec=5
TimeoutStopSec=5
ExecStart=/usr/bin/sh -c '$script'
EOF
}

# The template with the guard: SMBIOS strings read from sysinfo, and the
# virtio port whose host side is a file in this script's root-only directory.
# 1 when the template is not of the shape this expects (each anchor once, no
# SMBIOS of its own, no such port).
guarded_domain_xml() {
    local line os=0 devices=0 e
    if grep -q -e '<sysinfo' -e '<smbios' -e '<oemStrings' -e "name='$GUARD_PORT'" -- "$DOMAIN_XML"; then
        return 1
    fi
    while IFS= read -r line || [[ -n "$line" ]]; do
        case "$line" in
            "  </os>")
                os=$((os + 1))
                printf "    <smbios mode='sysinfo'/>\n%s\n" "$line"
                printf "  <sysinfo type='smbios'>\n    <oemStrings>\n"
                for e in "${GUARD_ENTRIES[@]}"; do
                    printf '      <entry>%s</entry>\n' "$e"
                done
                printf "    </oemStrings>\n  </sysinfo>\n"
                ;;
            "  </devices>")
                devices=$((devices + 1))
                printf "    <channel type='file'>\n      <source path='%s'/>\n      <target type='virtio' name='%s'/>\n    </channel>\n%s\n" \
                    "$GUARD_FILE" "$GUARD_PORT" "$line"
                ;;
            *) printf '%s\n' "$line" ;;
        esac
    done <"$DOMAIN_XML"
    ((os == 1 && devices == 1))
}

# What of the guard a domain definition ($1) lacks, as a list; empty when it
# carries all of it.
guard_missing() {
    local e missing=""
    for e in "${GUARD_ENTRIES[@]}"; do
        [[ "$1" == *">$e<"* ]] || missing+="${missing:+, }${e%%=*}"
    done
    [[ "$1" == *"<smbios mode='sysinfo'/>"* ]] || missing+="${missing:+, }<smbios mode='sysinfo'/>"
    if [[ "$1" != *"name='$GUARD_PORT'"* || "$1" != *"<source path='$GUARD_FILE'"* ]]; then
        missing+="${missing:+, }the $GUARD_PORT port to $GUARD_FILE"
    fi
    printf '%s' "$missing"
}

# Whether a domain definition ($1) carries any of a session guard.
guard_in() {
    [[ "$1" == *"name='$GUARD_PORT'"* || "$1" == *io.systemd.credential* ]]
}

# The session's guard state, beside its report: what the record said and the
# lines a guarded recovery OS writes; then when the domain was started and
# resumed. Key=value lines, the last of a key counting. status and
# session-end judge the report by it after this script is gone.
write_guard_state() {
    if ! (umask 077 && printf '%s\n' "$@" >>"$GUARD_STATE_FILE"); then
        refuse "cannot record the session guard's state in $GUARD_STATE_FILE -- nothing was booted"
    fi
}

# Read a guard state file ($1) into GS_VERDICT, GS_ENGAGED, GS_LIFTED (and
# their short forms GS_ENGAGED_C, GS_LIFTED_C), GS_STARTED, GS_RESUMED. 1
# when it is missing, unreadable, or incomplete.
read_guard_state() {
    local k v
    GS_VERDICT="" GS_ENGAGED="" GS_LIFTED="" GS_ENGAGED_C="" GS_LIFTED_C="" GS_STARTED="" GS_RESUMED=""
    [[ -f "$1" && ! -L "$1" ]] || return 1
    while IFS='=' read -r k v; do
        case "$k" in
            verdict) GS_VERDICT=$v ;;
            engaged) GS_ENGAGED=$v ;;
            lifted) GS_LIFTED=$v ;;
            engaged_short) GS_ENGAGED_C=$v ;;
            lifted_short) GS_LIFTED_C=$v ;;
            started) GS_STARTED=$v ;;
            resumed) GS_RESUMED=$v ;;
        esac
    done 2>/dev/null <"$1" || return 1
    [[ "$GS_VERDICT" =~ ^(will|may|no)$ && "$GS_ENGAGED" == "das-vm-guard engaged "* && "$GS_LIFTED" == "das-vm-guard lifted "* &&
        "$GS_ENGAGED_C" == "das-vm-guard engaged "* && "$GS_LIFTED_C" == "das-vm-guard lifted "* ]]
}

# Define the domain with the guard, and prove the definition carries it.
# Fail closed: without it, nothing boots.
define_guard() {
    local out xml missing
    check_path_chars "$GUARD_FILE"
    rm -f -- "$GUARD_FILE" "$GUARD_FILE".[0-9]* "$GUARD_STATE_FILE" "$RESET_FILE"
    if [[ -e "$GUARD_FILE" || -e "$GUARD_FILE.0" || -e "$GUARD_STATE_FILE" || -e "$RESET_FILE" ]]; then
        refuse "cannot remove an old $GUARD_FILE, its state or its resets: a stale report could pass for this session's -- nothing was booted"
    fi
    render_domain_xml || refuse "$RENDER_FAILURE -- nothing was booted"
    if ! (umask 077 && guarded_domain_xml >"$GUARD_XML_FILE"); then
        rm -f -- "$GUARD_XML_FILE" "$DOMAIN_XML"
        refuse "cannot add the session guard to $DOMAIN_XML (each of '  </os>' and '  </devices>' once, and no SMBIOS strings or $GUARD_PORT port of its own, are needed) -- nothing was booted"
    fi
    rm -f -- "$DOMAIN_XML"
    write_guard_state "verdict=$VERDICT" "engaged=$GUARD_EXPECT" "lifted=$GUARD_EXPECT_LIFTED" \
        "engaged_short=$GUARD_EXPECT_C" "lifted_short=$GUARD_EXPECT_LIFTED_C"
    # Set first: if the define half-happens, the cleanup must look.
    GUARDED=true
    if ! out="$(virsh_ define --validate "$GUARD_XML_FILE" 2>&1)"; then
        refuse "cannot define the session guard into $DOMAIN: $out -- nothing was booted"
    fi
    xml="$(virsh_ dumpxml --inactive "$DOMAIN" 2>&1)" || refuse "cannot read $DOMAIN's definition back: $xml -- nothing was booted"
    missing="$(guard_missing "$xml")"
    if [[ -n "$missing" ]]; then
        refuse "$DOMAIN's definition does not carry the session guard after defining it (missing: $missing) -- nothing was booted"
    fi
    log "defined $DOMAIN with the session guard (${#MASKS[@]} masks; its report goes to $GUARD_FILE)"
}

# Define the domain from the template again: no guard. 1, with GUARD_LEFT
# set, when that cannot be proven. Never a reason to keep the disk: the guard
# only keeps btrbk from running in that VM.
remove_guard() {
    local out xml
    if ! render_domain_xml; then
        GUARD_LEFT="$RENDER_FAILURE"
    elif ! out="$(virsh_ define --validate "$DOMAIN_XML" 2>&1)"; then
        GUARD_LEFT="virsh define $DOMAIN_XML failed: $out"
    elif ! xml="$(virsh_ dumpxml --inactive "$DOMAIN" 2>&1)"; then
        GUARD_LEFT="cannot read $DOMAIN's definition back: $xml"
    elif guard_in "$xml"; then
        GUARD_LEFT="$DOMAIN's definition still carries it after defining $DOMAIN_XML"
    else
        rm -f -- "$DOMAIN_XML"
        GUARDED=false
        # The reset watch goes before its file: it writes there, and a line
        # it wrote after this would leave the file behind (bd lorz).
        stop_reset_watch
        rm -f -- "$GUARD_XML_FILE" "$GUARD_FILE" "$GUARD_FILE".[0-9]* "$GUARD_STATE_FILE" "$RESET_FILE"
        log "took the session guard out of $DOMAIN's definition (defined from $DOMAIN_TEMPLATE again)"
        return 0
    fi
    rm -f -- "$DOMAIN_XML"
    warn "the session guard is still in $DOMAIN's definition: $GUARD_LEFT. It only keeps btrbk from running in that VM; once it is shut off, run: $SELF define"
    return 1
}

# The reader's place in a report (report_read): R_INO the file being read,
# R_OFF how much of it was read, R_STREAM how much of the whole report,
# R_BASES[inode] where each file read begins in it, R_PART an unfinished
# last line, R_POLL_START where this round's reading began.
R_INO="" R_OFF=0 R_STREAM=0 R_PART="" R_POLL_START=0
declare -A R_BASES=()

report_reader_start() {
    R_INO="" R_OFF=0 R_STREAM=0 R_PART="" R_POLL_START=0
    R_BASES=()
}

# Whether a report's path ($1) is one that cannot be read: a symlink, not a
# regular file, or a file that will not open. Absent is not that.
report_unreadable() {
    [[ -L "$1" ]] || { [[ -e "$1" ]] && { [[ ! -f "$1" ]] || ! head -c 0 -- "$1" 2>/dev/null; }; }
}

# Read what is new in the report $1: each complete line into NEW_LINES, and
# where it begins in the whole report into NEW_POS. The report is followed
# across virtlogd's rotation -- once the file reaches max_size, virtlogd
# renames it to .0 (.0 to .1, and so on: virrotatingfile.c) and starts a new
# one, cutting a line in two when none ends near the limit -- by inode: what
# is left of the file being read is read where it is now, then each newer
# file. Rotation loses nothing. A file cut in place (not virtlogd's way) is
# read again from its start. What cannot be read -- the file being read gone,
# or cut -- is said in R_LOST. A file renamed while it is read is read at the
# next look.
report_read() {
    local base=$1 k i f st data off cut start=-1 files=() inos=() sizes=() parts=() pos
    NEW_LINES=() NEW_POS=() R_LOST=""
    R_POLL_START=$R_STREAM
    for ((k = 0; k < REPORT_BACKUPS_MAX; k++)); do
        [[ -e "$base.$k" ]] || break
    done
    # Oldest first: .N ... .0, then the file itself.
    for ((i = k - 1; i >= 0; i--)); do
        files+=("$base.$i")
    done
    files+=("$base")
    for f in "${files[@]}"; do
        st="$(stat -c '%i %s' -- "$f" 2>/dev/null)" || st=" "
        inos+=("${st% *}")
        sizes+=("${st#* }")
    done
    if [[ -n "$R_INO" ]]; then
        for i in "${!inos[@]}"; do
            [[ "${inos[$i]}" != "$R_INO" ]] || start=$i
        done
    fi
    if ((start < 0)); then
        # Nothing read yet, or the file being read is gone: the oldest file
        # not read yet.
        for i in "${!inos[@]}"; do
            if [[ -n "${inos[$i]}" && -z "${R_BASES[${inos[$i]}]:-}" ]]; then
                start=$i
                break
            fi
        done
        ((start >= 0)) || return 0
        if [[ -n "$R_INO" ]]; then
            R_LOST="the rest of the file being read (inode $R_INO) was gone before it could be read; lines in it would never be seen"
            R_INO="" R_OFF=0 R_PART=""
        fi
    fi
    for ((i = start; i < ${#files[@]}; i++)); do
        f=${files[$i]} off=0 cut=false
        [[ -n "${inos[$i]}" ]] || continue
        if [[ "${inos[$i]}" == "$R_INO" ]]; then
            off=$R_OFF
            if ((${sizes[$i]} < R_OFF)); then
                cut=true off=0
            fi
        elif [[ -n "${R_BASES[${inos[$i]}]:-}" ]]; then
            continue
        fi
        # Kept whole even when it ends in newlines; and still the same file.
        data="$(tail -c "+$((off + 1))" -- "$f" 2>/dev/null && printf x)" || data=""
        if [[ "$data" != *x || "$(stat -c %i -- "$f" 2>/dev/null)" != "${inos[$i]}" ]]; then
            break
        fi
        data=${data%x}
        if [[ "${inos[$i]}" != "$R_INO" ]]; then
            R_INO=${inos[$i]}
            R_BASES[$R_INO]=$R_STREAM
        elif [[ "$cut" == true ]]; then
            R_LOST="the report was cut in place, from $R_OFF bytes to ${sizes[$i]}: whatever it held past that, unread, would never be seen"
            R_BASES[$R_INO]=$R_STREAM
            R_PART=""
        fi
        R_OFF=$((off + ${#data}))
        R_STREAM=$((R_BASES[$R_INO] + R_OFF))
        data=$R_PART$data
        pos=$((R_STREAM - ${#data}))
        # The last of the parts is what follows the last newline.
        mapfile -t parts <<<"$data"
        for ((k = 0; k < ${#parts[@]} - 1; k++)); do
            NEW_LINES+=("${parts[$k]}")
            NEW_POS+=("$pos")
            pos=$((pos + ${#parts[$k]} + 1))
        done
        R_PART=${parts[${#parts[@]} - 1]}
    done
}

# Begin judging a report: $1..$4 the lines a guarded recovery OS writes --
# engaged, engaged in short, lifted, lifted in short. The judgement so far:
# J_RESULT none (no line yet), ok, or failed (final; J_WHY says why); J_LINES,
# J_BOOTS, J_LIFTS (times lifted); the boot the last line came from (J_BOOT),
# its last number (J_SEQ) and state (J_MODE); J_LOST (lines of a boot that
# never arrived), J_RESTARTS (its reporter started again), J_ODD (lines out of
# order).
judge_start() {
    J_E=$1 J_EC=$2 J_L=$3 J_LC=$4
    J_RESULT=none J_WHY="" J_LINES=0 J_BOOTS=0 J_LIFTS=0 J_SEEN=" " J_BOOT="" J_SEQ=0 J_MODE=""
    J_LOST=0 J_RESTARTS=0 J_ODD=0 J_PARTIAL="" J_NEW_BOOT=false J_NOTE=""
}

# Judge one line of the report ($1). A line is "<what> seq <n> boot <id>".
# Each boot must begin with its line 1, engaged; its later lines, engaged or
# lifted. A NOT engaged line, or a line of any other shape, fails -- final.
# So does a boot first seen past its line 1: that line was lost, and whether
# the boot began guarded cannot be known. Within a boot, lines that never
# arrived, came twice or out of order are noted in J_NOTE, never a failure:
# the boot was seen to begin engaged. J_NEW_BOOT: this line began a boot.
judge_line() {
    local line=$1 body seq boot mode full
    J_NEW_BOOT=false J_NOTE=""
    [[ "$J_RESULT" != failed ]] || return 0
    J_LINES=$((J_LINES + 1))
    if [[ ! "$line" =~ $REPORT_LINE_RE ]]; then
        J_RESULT=failed J_WHY="a report line that is no report: '$(printable "$line")'"
        return 0
    fi
    body=${BASH_REMATCH[1]} seq=${BASH_REMATCH[2]} boot=${BASH_REMATCH[3]}
    case "$body" in
        "das-vm-guard NOT engaged:"*)
            J_RESULT=failed J_WHY="the recovery OS reports it NOT engaged:$(printable "${body#das-vm-guard NOT engaged:}") (boot $boot)"
            return 0
            ;;
        "$J_E") mode=engaged full=true ;;
        "$J_EC") mode=engaged full=false ;;
        "$J_L") mode=lifted full=true ;;
        "$J_LC") mode=lifted full=false ;;
        *)
            J_RESULT=failed J_WHY="a report line that is no report: '$(printable "$line")'"
            return 0
            ;;
    esac
    if [[ "$boot" != "$J_BOOT" && "$J_SEEN" == *" $boot "* ]]; then
        J_ODD=$((J_ODD + 1))
        J_NOTE="a line of boot $boot came after boot $J_BOOT began"
    elif [[ "$boot" != "$J_BOOT" ]]; then
        if ((seq != 1)); then
            J_RESULT=failed J_WHY="boot $boot: its first report was not seen (this is its line $seq) -- lines of the report were lost, so whether that boot began with the guard engaged cannot be known"
            return 0
        fi
        if [[ "$mode" != engaged ]]; then
            J_RESULT=failed J_WHY="boot $boot began without the guard engaged: '$(printable "$body")'"
            return 0
        fi
        J_SEEN+="$boot "
        J_BOOT=$boot J_SEQ=1 J_MODE=engaged J_NEW_BOOT=true
        J_BOOTS=$((J_BOOTS + 1))
    else
        if ((seq > J_SEQ + 1)); then
            J_LOST=$((J_LOST + seq - J_SEQ - 1))
            J_NOTE="$(count_of $((seq - J_SEQ - 1)) line) of boot $boot never arrived (its $((J_SEQ + 1)) to $((seq - 1))): a NOT engaged one among them would not have been seen"
        elif ((seq < J_SEQ)); then
            J_ODD=$((J_ODD + 1))
            J_NOTE="a line of boot $boot came out of order (its $seq after its $J_SEQ)"
        elif [[ "$full" == true && "$mode" == "$J_MODE" ]]; then
            # A reporter names the masks in full when it starts.
            J_RESTARTS=$((J_RESTARTS + 1))
            J_NOTE=restarted
        fi
        if [[ "$mode" == lifted && "$J_MODE" != lifted ]]; then
            J_LIFTS=$((J_LIFTS + 1))
        fi
        ((seq <= J_SEQ)) || J_SEQ=$seq
        J_MODE=$mode
    fi
    J_RESULT=ok
}

# Judge a saved report ($1) from its beginning -- every file of it, oldest
# first -- with the resets its session's reset watch recorded ($6), as a
# session judges them (judge_stream); $2..$5 as judge_start's. A reset whose
# place in the report is not one read (its file is gone) is placed at the
# report's end: only a boot after everything there can answer it. J_LAST_AT:
# when its newest file was last written (seconds since the epoch), empty when
# none is there.
judge_report_files() {
    local f
    J_LAST_AT=""
    judge_start "$2" "$3" "$4" "$5"
    report_reader_start
    RESETS=0 RESET_LINES_READ=0 PROOF_WHY=start PROOF_POS=0 PROOF_AT=0 PROOF_DEADLINE=0 GCLOCK=0
    if report_unreadable "$1"; then
        J_RESULT=failed J_WHY="the report $1 cannot be read"
        return 0
    fi
    report_read "$1"
    read_resets "$6" "$R_STREAM"
    judge_stream saved
    [[ "$J_RESULT" != failed ]] || return 0
    J_PARTIAL="$(printable "$R_PART")"
    for f in "$1" "$1.0"; do
        if [[ -f "$f" ]]; then
            J_LAST_AT="$(stat -c %Y -- "$f" 2>/dev/null)" || J_LAST_AT=""
            break
        fi
    done
}

# "engaged (2 boots; lifted 1 time)", from the last judgement.
judged_engaged() {
    printf 'engaged (%s; lifted %s)' "$(count_of "$J_BOOTS" boot)" "$(count_of "$J_LIFTS" time)"
}

# The guard did not hold, for $1. On a "will" or "may" record a running
# recovery OS ($2) is asked to shut down; on "no" it is said.
guard_failed() {
    GUARD_FAILED=true
    GUARD_RESULT="NOT confirmed: $1"
    if [[ "$VERDICT" == no ]]; then
        warn "the session guard did not confirm: $1. The boot record says btrbk will not run when this OS boots, so the session goes on -- see the summary"
        return 0
    fi
    warn "the session guard did not confirm: $1. The boot record says btrbk $VERDICT run when this OS boots, and nothing now vouches that it cannot"
    if [[ "$2" == running ]]; then
        GUARD_SHUT_DOWN=true
        request_shutdown "the session guard did not confirm" guard
    fi
}

# What is wrong with the report, said once each: lines lost or out of order.
# Never a failure -- every boot was seen to begin engaged -- but the summary
# carries it.
report_trouble() {
    REPORT_LOSSES+=("$1")
    warn "the session guard's report: $1"
}

# "THE SESSION GUARD'S REPORTER IS SILENT ...", $1 seconds of it.
silence_text() {
    printf '%s' "THE SESSION GUARD'S REPORTER IS SILENT: nothing from the recovery OS for $(format_duration "$1") (its last report from boot ${J_BOOT:-unknown}), and the VM has not been reset since. Either its reporter stopped -- the guard itself, the bind mount and the masks, does not depend on it -- or the recovery OS has hung: this script cannot tell which, and does not stop it for that. Look: virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN (inside it: systemctl status das-vm-guard-report das-vm-guard)"
}

# Advance the guard's clock to now ($1: the domain's state now) -- by none
# of the time it is paused, and none of a gap between two looks far longer
# than a look takes: a host suspend (the recovery OS did not run either), or
# this script stopped. A gap is said, and -- unless the domain read paused
# at the looks on both sides of it -- counted for the summary (G_GAP_SECS):
# this script cannot tell a host suspend from its own stop (a Ctrl-Z), and
# in the second the recovery OS ran that long unwatched.
guard_clock_tick() {
    local now=$SECONDS d before=$G_TICK_STATE
    d=$((now - G_TICK_AT))
    G_TICK_AT=$now G_TICK_STATE=$1
    ((d > 0)) || return 0
    if ((d > CLOCK_GAP_SECS)); then
        if [[ "$before" != paused || "$1" != paused ]]; then
            G_GAPS=$((G_GAPS + 1)) G_GAP_SECS=$((G_GAP_SECS + d))
        fi
        log "$(format_duration "$d") passed between two looks at the recovery OS (a host suspend, or this script was stopped): the guard's deadlines and its silence do not count that time"
        return 0
    fi
    [[ "$1" == paused ]] || GCLOCK=$((GCLOCK + d))
}

# Begin watching the guard: just before the domain is resumed. The first
# boot must report within GUARD_SECS.
guard_watch_begin() {
    judge_start "$GUARD_EXPECT" "$GUARD_EXPECT_C" "$GUARD_EXPECT_LIFTED" "$GUARD_EXPECT_LIFTED_C"
    report_reader_start
    GCLOCK=0 G_TICK_AT=$SECONDS G_LAST_LINE=0 G_LIFTS_SAID=0
    PROOF_WHY=start PROOF_POS=0 PROOF_DEADLINE=$GUARD_SECS
}

# The guard's judgement of what is new -- the report's lines (NEW_LINES,
# NEW_POS) and the resets the reset watch recorded (NEW_RESETS, NEW_RESET_AT)
# -- in the order they happened: a reset recorded where the report stood
# comes before every line that begins there or later. The one judgement of
# the guard: a session's looks (check_guard, $1 "live": each reset and line is
# said, and timed on the guard's clock) and status and session-end
# (judge_report_files, $1 "saved") both make it, and read the outcome the same
# way. Stops at a line that fails (J_RESULT failed, J_WHY). Afterwards
# PROOF_WHY says what has still to prove itself: "start" (the first boot),
# "reset" (a boot whose first line begins at or after PROOF_POS, the place of
# the last reset; the first reset not answered was at PROOF_AT, seconds since
# the epoch, and PROOF_DEADLINE on the guard's clock), or nothing. A boot
# loop of resets does not push that deadline out.
judge_stream() {
    local i=0 j=0
    while ((i < ${#NEW_LINES[@]} || j < ${#NEW_RESETS[@]})); do
        if ((j < ${#NEW_RESETS[@]})) && { ((i >= ${#NEW_LINES[@]})) || ((NEW_RESETS[j] <= NEW_POS[i])); }; then
            RESETS=$((RESETS + 1))
            if [[ -z "$PROOF_WHY" ]]; then
                PROOF_WHY=reset PROOF_AT=${NEW_RESET_AT[$j]} PROOF_DEADLINE=$((GCLOCK + GUARD_SECS))
            fi
            PROOF_POS=${NEW_RESETS[$j]}
            if [[ "$1" == live ]]; then
                guard_reset_seen
            fi
            j=$((j + 1))
            continue
        fi
        judge_line "${NEW_LINES[$i]}"
        [[ "$J_RESULT" != failed ]] || return 0
        PROOF_ANSWERED=""
        if [[ "$J_NEW_BOOT" == true && -n "$PROOF_WHY" ]] && ((NEW_POS[i] >= PROOF_POS)); then
            PROOF_ANSWERED=$PROOF_WHY PROOF_WHY=""
        fi
        if [[ "$1" == live ]]; then
            guard_line_seen
        fi
        i=$((i + 1))
    done
}

# A line that judge_line found good, in a session.
guard_line_seen() {
    local silent=$((GCLOCK - G_LAST_LINE))
    G_LAST_LINE=$GCLOCK
    if [[ "$SILENT" == true ]]; then
        SILENT=false
        log "the recovery OS reports again, after $(format_duration "$silent") of silence"
    fi
    if [[ "$PROOF_ANSWERED" == reset ]]; then
        log "the boot after the reset reports its guard engaged (boot $J_BOOT)"
    fi
    if [[ "$GUARD_CONFIRMED" != true ]]; then
        GUARD_CONFIRMED=true
        log "the session guard is engaged: $GUARD_EXPECT"
        if [[ "$WILL_BANNER" == true ]]; then
            warn "$WILL_BANNER_TEXT"
        fi
    elif [[ "$J_NEW_BOOT" == true ]]; then
        log "the recovery OS booted again (boot $J_BOOT), and its guard is engaged ($(count_of "$J_BOOTS" boot) so far)"
    fi
    if ((J_LIFTS > G_LIFTS_SAID)); then
        G_LIFTS_SAID=$J_LIFTS
        log "the session guard is lifted for the update (its masks hold); btrbk can run there by hand until it is engaged again"
    fi
    case "$J_NOTE" in
        "") ;;
        restarted) log "the guard's reporter in the recovery OS started again (boot $J_BOOT); the guard does not depend on it" ;;
        *) report_trouble "$J_NOTE" ;;
    esac
}

# A reset of the VM, in a session (judge_stream has set what must prove
# itself): said; the silence before it is over.
guard_reset_seen() {
    SILENT=false
    log "the recovery OS was reset (a reboot inside it, or virsh reset): a boot after that must report its guard engaged within $(format_duration $((PROOF_DEADLINE - GCLOCK)))"
}

# Judge what is new: the report's lines and the resets the reset watch saw,
# in the order they happened -- a reset recorded where the report stood comes
# before every line that begins there or later. $1: "running" (the domain
# runs, and may be asked to shut down), "off" (it is off: the last look), or
# "final" (this script is leaving: judge, never act). $2: its state now.
#   - A line that fails is final (guard_failed).
#   - The first boot must report within GUARD_SECS of the start, and after a
#     reset, a boot that begins after it within GUARD_SECS of the reset --
#     by the guard's clock. Otherwise guard_failed: a boot that came without
#     the guard says nothing.
#   - Silence past GUARD_SECS with no reset since: the guard's enforcement
#     does not depend on its reporter, so a warning, said again every
#     GUARD_SECS -- never a stop. Unless the reset watch is down: a reset
#     cannot be ruled out then, and silence past GUARD_SECS from the later of
#     the last line and the watch's end fails as above.
#   - Off: no line at all, or a reset no boot answered, fails; so does a last
#     line left unfinished that can only be a NOT engaged one, and any other
#     unfinished last line is said (report_trouble).
#   - Final: a deadline already passed fails (guard_failed never acts then).
check_guard() {
    local mode=$1 state=${2:-} partial="" silent resets
    if [[ "$GUARD_FAILED" == true ]]; then
        return 0
    fi
    guard_clock_tick "$state"
    if report_unreadable "$GUARD_FILE"; then
        guard_failed "the report $GUARD_FILE cannot be read" "$mode"
        return 0
    fi
    report_read "$GUARD_FILE"
    if [[ -n "$R_LOST" ]]; then
        report_trouble "$R_LOST"
    fi
    if [[ "$mode" == running ]]; then
        reset_watch_check
    fi
    read_resets "$RESET_FILE" "$R_POLL_START"
    judge_stream live
    if [[ "$J_RESULT" == failed ]]; then
        guard_failed "$J_WHY" "$mode"
        return 0
    fi
    J_PARTIAL="$(printable "$R_PART")"
    if [[ -n "$J_PARTIAL" ]]; then
        partial=" (an unfinished line: '$J_PARTIAL')"
    fi
    if ((J_LINES > 0)); then
        resets=""
        # Not inside the assignment: there a false test would be its status.
        if ((RESETS > 0)); then
            resets="; $(count_of "$RESETS" reset)"
        fi
        GUARD_RESULT="engaged -- $GUARD_EXPECT ($(count_of "$J_BOOTS" boot); lifted $(count_of "$J_LIFTS" time)$resets)"
    fi
    case "$mode" in
        off)
            if ((J_LINES == 0)); then
                guard_failed "the recovery OS powered off without reporting$partial" off
            elif [[ "$PROOF_WHY" == reset ]]; then
                guard_failed "the recovery OS was reset, and powered off before a boot after that reported its guard engaged$partial -- a boot that came without the guard says nothing (or it never got as far as its OS)" off
            elif [[ "$R_PART" == "das-vm-guard N"* ]]; then
                # Only a NOT engaged line begins so; cut short, it is still one.
                guard_failed "the recovery OS powered off in the middle of a line that reports it NOT engaged: '$J_PARTIAL'" off
            elif [[ -n "$J_PARTIAL" ]]; then
                report_trouble "the recovery OS powered off in the middle of a line, never finished: '$J_PARTIAL' -- whatever the rest of it said was never seen"
            fi
            return 0
            ;;
    esac
    # A deadline passed is judged at the last look too ("final"), never acted on.
    if [[ -n "$PROOF_WHY" ]] && ((GCLOCK >= PROOF_DEADLINE)); then
        if [[ "$PROOF_WHY" == start ]]; then
            guard_failed "no report from the recovery OS within $(format_duration "$GUARD_SECS") of its start$partial -- an OS whose systemd is older than 256 does not even see the guard" "$mode"
        else
            guard_failed "the recovery OS was reset, and no boot after that reported its guard engaged within $(format_duration "$GUARD_SECS")$partial -- a boot that came without the guard says nothing" "$mode"
        fi
        return 0
    fi
    if [[ -n "$PROOF_WHY" || "$mode" == final ]]; then
        return 0
    fi
    if [[ -n "$RESET_WATCH_DOWN" ]]; then
        silent=$((GCLOCK - (G_LAST_LINE > G_WATCH_DOWN_AT ? G_LAST_LINE : G_WATCH_DOWN_AT)))
        if ((silent >= GUARD_SECS)); then
            guard_failed "the recovery OS stopped reporting: nothing for $(format_duration "$silent"), and with the reset watch down ($RESET_WATCH_DOWN) a boot that came without the guard cannot be told from a reporter that stopped" running
        fi
        return 0
    fi
    silent=$((GCLOCK - G_LAST_LINE))
    if ((silent < GUARD_SECS)); then
        return 0
    fi
    if [[ "$SILENT" != true ]]; then
        SILENT=true SILENCES=$((SILENCES + 1)) SILENT_NEXT=$GCLOCK
    fi
    ((silent <= SILENCE_LONGEST)) || SILENCE_LONGEST=$silent
    if ((GCLOCK >= SILENT_NEXT)); then
        warn "$(silence_text "$silent")"
        SILENT_NEXT=$((GCLOCK + GUARD_SECS))
    fi
}

# The judgement of a session's guard for status and session-end, from its
# state ($1, the state file), report ($2) and the resets its reset watch
# recorded ($4): GJ_TEXT, and GJ_STATUS -- 0, or 6 (5 on a "no" record) when
# it is not engaged or cannot be judged. $3 is the domain's state. The resets
# are judged as the session judges them (judge_stream): a reset that no boot
# after it answered is NOT confirmed once the domain is shut off, or once
# GUARD_SECS have passed since it (by the wall clock; never while paused) --
# and pending before that. With no driver left to watch for later resets,
# silence counts here as it cannot in a session: a recovery OS not shut off
# (nor paused) whose report was last written GUARD_SECS ago or more is NOT
# confirmed. An unfinished last line of one shut off is said (5), and is NOT
# engaged when only a NOT engaged line begins so. Information only: nothing
# here stops anything.
judge_saved_guard() {
    local partial="" age before
    GJ_STATUS=0
    if ! read_guard_state "$1"; then
        GJ_TEXT="cannot be judged: its session state ($1) is gone or unreadable -- a host restart clears /run"
        GJ_STATUS=6
        return 0
    fi
    judge_report_files "$2" "$GS_ENGAGED" "$GS_ENGAGED_C" "$GS_LIFTED" "$GS_LIFTED_C" "$4"
    if [[ -n "$J_PARTIAL" ]]; then
        partial=" (an unfinished line: '$J_PARTIAL')"
    fi
    if [[ "$J_RESULT" == failed ]]; then
        GJ_TEXT="NOT engaged: $J_WHY"
    elif [[ "$J_RESULT" == ok && "$PROOF_WHY" == reset ]]; then
        before="$(judged_engaged)"
        if [[ "$3" == "shut off" ]]; then
            GJ_TEXT="NOT confirmed: the recovery OS was reset, and was shut off before a boot after that reported its guard engaged$partial -- a boot that came without the guard says nothing (before the reset: $before)"
        elif [[ "$3" == paused ]]; then
            GJ_TEXT="pending: the recovery OS was reset, and no boot after that has reported its guard engaged yet; it is paused, and time paused does not count -- until a boot after the reset reports engaged, treat it as unguarded (before the reset: $before)"
            return 0
        else
            age=$(($(date +%s) - PROOF_AT))
            if ((age < GUARD_SECS)); then
                GJ_TEXT="pending: the recovery OS was reset $(format_duration "$age") ago, and no boot after that has reported its guard engaged yet; one must within $(format_duration "$GUARD_SECS") of the reset -- until it does, treat it as unguarded (before the reset: $before)"
                return 0
            fi
            GJ_TEXT="NOT confirmed: the recovery OS was reset $(format_duration "$age") ago, and no boot after that reported its guard engaged within $(format_duration "$GUARD_SECS")$partial -- a boot that came without the guard says nothing (before the reset: $before). Look: virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN"
        fi
    elif [[ "$J_RESULT" == ok && "$3" == "shut off" && "$R_PART" == "das-vm-guard N"* ]]; then
        GJ_TEXT="NOT engaged: the recovery OS was shut off in the middle of a line that reports it NOT engaged: '$J_PARTIAL'"
    elif [[ "$J_RESULT" == ok ]]; then
        GJ_TEXT="$(judged_engaged)"
        if ((RESETS > 0)); then
            GJ_TEXT+="; each of its $(count_of "$RESETS" reset) answered by a boot reporting engaged"
        fi
        if ((J_LOST > 0)); then
            GJ_TEXT+="; $(count_of "$J_LOST" line) of its report never arrived"
        fi
        if [[ "$3" == "shut off" && -n "$J_PARTIAL" ]]; then
            GJ_TEXT+="; it was shut off in the middle of a line, never finished: '$J_PARTIAL'"
            GJ_STATUS=5
            return 0
        fi
        if [[ "$3" == "shut off" || "$3" == paused || ! "$J_LAST_AT" =~ ^[[:digit:]]+$ ]]; then
            return 0
        fi
        age=$(($(date +%s) - J_LAST_AT))
        if ((age < GUARD_SECS)); then
            GJ_TEXT+="; its last report $(format_duration "$age") ago"
            return 0
        fi
        GJ_TEXT="NOT confirmed: silent for $(format_duration "$age") -- the reporter stopped, or a boot came without the guard (status cannot see a reset made after the session's driver ended; before the silence: $GJ_TEXT). Look: virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN"
    elif [[ -z "$GS_RESUMED" ]]; then
        if [[ -n "$GS_STARTED" ]]; then
            GJ_TEXT="never ran (started paused, never resumed): nothing to judge"
        else
            GJ_TEXT="never ran (never started): nothing to judge"
        fi
        return 0
    elif [[ "$3" != "shut off" && "$GS_RESUMED" =~ ^[[:digit:]]+$ ]] && (($(date +%s) - GS_RESUMED < GUARD_SECS)); then
        GJ_TEXT="no report yet ($(format_duration $(($(date +%s) - GS_RESUMED))) since it started; it has $(format_duration "$GUARD_SECS"))"
        return 0
    else
        GJ_TEXT="NOT engaged: no report from a recovery OS that ran$partial"
    fi
    GJ_STATUS=6
    if [[ "$GS_VERDICT" == no ]]; then
        GJ_STATUS=5
    fi
}

# PERMITTED ONLY HERE -- the operator-approved exception in the header -- and
# the only `virsh destroy` this script ever runs: a domain started with
# --paused --force-boot and never resumed has not executed a single guest
# instruction -- no firmware, no boot loader, no OS, no write to the disk. It
# is not a running recovery OS, and tearing it down interrupts nothing. Once
# `virsh resume` has been tried, never: a resumed recovery OS may be in the
# middle of anything, and only ACPI may ask it to stop. And only when one
# read, right here, says both that it is paused and that its vCPUs have never
# run (`virsh domstats --state --vcpu`: state.state 3, and every online vCPU's
# vcpu.N.time 0). Paused alone is not enough: something else may have resumed
# it and paused it again (virsh suspend, or an I/O error under
# error_policy=stop), and domstate cannot tell that from a start --paused;
# the vCPUs' time can. A read that fails, or lacks a vCPU's time (libvirt
# leaves the vCPU fields out when it cannot read them), destroys nothing.
# What is left (bd DAS-Backup-Manager-ir5c): libvirt counts a vCPU's time
# from its thread's utime + stime (/proc/<pid>/task/<tid>/stat, whole clock
# ticks: 10 ms), so a resume and pause by someone else with less than 10 ms
# of vCPU time in between reads 0, and so does a thread whose stat libvirt
# could not parse (it warns, and reports 0); and a resume landing between
# the read and the destroy -- two virsh calls, milliseconds.
#   0 destroyed; 1 a resume was tried; 2 not paused (DESTROY_STATE says what
#   it is); 3 virsh destroy failed; 4 its state or its vCPUs' time could not
#   be read (DESTROY_STATE says why); 5 paused, but its vCPUs have run.
# 2 and 5 set MAY_HAVE_RUN: something other than this script resumed it.
destroy_never_resumed() {
    local out stats line n=0 ran=0 state="" current="" bad=false
    if [[ "$RESUME_ATTEMPTED" != false ]]; then
        warn "not destroying $DOMAIN: it was resumed, so it may have run"
        return 1
    fi
    if ! stats="$(virsh_ domstats --state --vcpu "$DOMAIN" 2>&1)"; then
        DESTROY_STATE="unknown (virsh domstats: $(printable "$stats"))"
        warn "not destroying $DOMAIN: whether it ever ran cannot be read -- $DESTROY_STATE"
        return 4
    fi
    while IFS= read -r line; do
        line=${line#"${line%%[![:space:]]*}"}
        case "$line" in
            state.state=*) state=${line#state.state=} ;;
            vcpu.current=*) current=${line#vcpu.current=} ;;
            vcpu.*.time=*)
                n=$((n + 1))
                if [[ ! "${line#*=}" =~ ^[0123456789]+$ ]]; then
                    bad=true
                elif [[ "${line#*=}" != 0 ]]; then
                    ran=$((ran + 1))
                fi
                ;;
        esac
    done <<<"$stats"
    case "$state" in
        0) DESTROY_STATE="no state" ;;
        1) DESTROY_STATE=running ;;
        2) DESTROY_STATE=idle ;;
        3) DESTROY_STATE=paused ;;
        4) DESTROY_STATE="in shutdown" ;;
        5) DESTROY_STATE="shut off" ;;
        6) DESTROY_STATE=crashed ;;
        7) DESTROY_STATE=pmsuspended ;;
        *)
            DESTROY_STATE="unknown (virsh domstats gave no state: '$(printable "$state")')"
            warn "not destroying $DOMAIN: whether it ever ran cannot be read -- $DESTROY_STATE"
            return 4
            ;;
    esac
    if [[ "$DESTROY_STATE" != paused ]]; then
        MAY_HAVE_RUN=true
        warn "not destroying $DOMAIN: it is $DESTROY_STATE, not paused -- this script never resumed it, so something else did (or it stopped), and it may have run"
        return 2
    fi
    if [[ "$bad" == true || ! "$current" =~ ^[123456789][0123456789]*$ ]] || ((n != current)); then
        DESTROY_STATE="paused, its vCPUs' time unknown (virsh domstats gave $n vCPU time(s) for vcpu.current '$(printable "$current")'$([[ "$bad" != true ]] || printf ', one not a number'))"
        warn "not destroying $DOMAIN: whether it ever ran cannot be read -- $DESTROY_STATE"
        return 4
    fi
    if ((ran > 0)); then
        MAY_HAVE_RUN=true
        DESTROY_STATE="paused, but its vCPUs have run ($ran of $n with CPU time)"
        warn "not destroying $DOMAIN: it is $DESTROY_STATE -- this script never resumed it, so something else resumed it and paused it again, and it may have run"
        return 5
    fi
    if ! out="$(virsh_ destroy "$DOMAIN" 2>&1)"; then
        warn "virsh destroy (of the paused, never resumed domain): $out"
        return 3
    fi
    DESTROYED=true
    log "destroyed $DOMAIN while still paused, its vCPUs never run, before it ran a single instruction"
}

# What the operator is told when destroy_never_resumed has destroyed the
# domain: that nothing ran, that a retry is safe, and what a repeat means.
never_ran_text() {
    printf '%s' "Nothing ran in $DOMAIN: it was destroyed while still paused, before it ran a single instruction -- no firmware, no OS, no write to the disk. A retry is safe: run the session again. If it fails the same way again, the cause is persistent -- libvirt not applying the session guard's SMBIOS strings to the started domain, for one -- and needs a fix before any session can boot"
}

# Whether process $1 (a child of this script) has ended: gone, or a zombie.
child_gone() {
    local s
    s="$(cat "/proc/$1/stat" 2>/dev/null)" || return 0
    s=${s##*) }
    [[ "${s%% *}" == Z ]]
}

# The reset watch: libvirt's event stream of this domain's resets (a reboot
# inside it, or virsh reset), read by a process of this script's own. At each
# reset it records where the report stood -- its file's inode and size -- so
# the boot after it is told by where its first line begins, not by when this
# script looked. Started before the domain; ended on every way out
# (stop_reset_watch). It ends by itself when the stream does (a libvirt
# restart; the reason is in RESET_FILE) or this script is gone.
#
# A line is taken whole: `read -t` that times out mid-line (this process
# stopped, or starved, while an event waited in the pipe) keeps what it read
# so far, and the rest is joined to it -- never filed apart, where neither
# half would be the event. And output not recognised that names a reboot at
# all is taken as a reset too: a proof is asked for rather than a reset lost.
# Each reset is recorded as "reset <inode> <size> <seconds since the epoch>".
#
# The virsh is started by this script, not by the watch (start_reset_watch),
# and the watch reads it on descriptor $1: so its pid is this script's from
# the moment it exists, and stop_reset_watch can always find and end it. A
# watch that started it itself held its pid alone until it wrote it down, and
# a signal in between left that virsh running with no one to end it (bd lorz).
reset_watch() {
    local line st wfd=$1 buf="" rc
    set +e
    # Ended by stop_reset_watch, which then ends the virsh itself.
    trap 'exit 0' TERM INT HUP
    # The maintenance lock is held by this script and the disk holder, never
    # by the watch or its virsh: inherited, its descriptor would keep the lock
    # held after this script let it go, for as long as the watch lived.
    if [[ -n "$LOCK_FD" ]]; then
        exec {LOCK_FD}>&-
    fi
    while :; do
        line=""
        IFS= read -r -t "$POLL_SECS" -u "$wfd" line
        rc=$?
        if ((rc > 128)); then
            # Timed out: what it read of an unfinished line is kept.
            buf+=$line
        else
            # A whole line -- or, at the stream's end (rc 1), what was left.
            line=$buf$line buf=""
            if [[ "$line" == *"$RESET_EVENT"* || "$line" == *reboot* ]]; then
                st="$(stat -c '%i %s' -- "$GUARD_FILE" 2>/dev/null)" || st="- 0"
                printf 'reset %s %(%s)T\n' "$st" -1 >>"$RESET_FILE"
            fi
            if [[ -n "$line" && "$line" != *"$RESET_EVENT"* ]]; then
                printf 'said %s\n' "$(printable "$line")" >>"$RESET_FILE"
            fi
            ((rc == 0)) || break
        fi
        if ! kill -0 "$$" 2>/dev/null; then
            # This script is gone without ending the virsh: no one else will.
            kill "$RESET_VIRSH_PID" 2>/dev/null
            break
        fi
    done
    # Else the stream ended: its virsh is gone, and this script reaps it.
    exit 0
}

# Start the virsh, then the watch that reads it. The virsh's pid is taken
# from $! at once; an interrupt between its start and that (the traps run
# between commands) reaches on_exit with RESET_VIRSH_STARTING still true, and
# stop_reset_watch takes it from $! there -- as on_exit does for the holder --
# if $! has moved from what it was before (RESET_VIRSH_BEFORE).
# Its pid goes in RESET_FILE too, before the domain starts: the file then
# exists for the whole session, and says which virsh it was.
start_reset_watch() {
    local wfd
    (umask 077 && : >>"$RESET_FILE") || refuse "cannot create $RESET_FILE -- nothing was booted"
    RESET_VIRSH_BEFORE=${!:-}
    RESET_VIRSH_STARTING=true
    exec {wfd}< <(
        # The maintenance lock is never the virsh's to hold (see reset_watch).
        if [[ -n "$LOCK_FD" ]]; then
            exec {LOCK_FD}>&-
        fi
        exec virsh --connect "$LIBVIRT_URI" event --domain "$DOMAIN" --event reboot --loop 2>&1
    )
    RESET_VIRSH_PID=$!
    RESET_VIRSH_STARTING=false
    # Information only: the pid this script needs is the one it holds.
    printf 'virsh %s\n' "$RESET_VIRSH_PID" >>"$RESET_FILE" || :
    reset_watch "$wfd" &
    RESET_WATCH_PID=$!
    # The watch's copy is the one read; this script's, closed, is not passed
    # on to anything it starts later.
    exec {wfd}<&-
    log "watching $DOMAIN for resets (libvirt's reboot event): a boot after one must report its guard engaged within $(format_duration "$GUARD_SECS")"
}

# Whether process $1, the reset watch's virsh, still runs: there, not a
# zombie, and still this script's child -- a pid freed once bash reaped it
# and taken by another process is not. Not by its command line: forked but
# not yet become virsh (an interrupt in that instant), it still reads as
# this script, and will run all the same.
reset_virsh_runs() {
    local st ppid
    [[ -n "$1" ]] || return 1
    { read -r st <"/proc/$1/stat"; } 2>/dev/null || return 1
    st=${st##*) }
    [[ "${st%% *}" != Z ]] || return 1
    read -r _ ppid _ <<<"$st"
    [[ "$ppid" == "$$" ]]
}

# End the reset watch, then the virsh it reads, and wait until each has --
# five seconds each at most, then killed, and waited for again. The virsh is
# this script's own child (start_reset_watch): SIGTERM from here, its time to
# end on it, then SIGKILL, and reaped. Before bd lorz the virsh was the
# watch's child, its pid known here only from RESET_FILE: killed at once, cut
# short while still ending on a loaded host; not found at all where
# remove_guard had already taken that file, or where a signal reached the
# watch before it wrote the pid down. Either way the driver went on before it
# had gone, or left it running. The watch still goes before its file
# (remove_guard): it writes there.
stop_reset_watch() {
    local i
    if [[ "$RESET_VIRSH_STARTING" == true && -z "$RESET_VIRSH_PID" && "${!:-}" != "$RESET_VIRSH_BEFORE" ]]; then
        # Interrupted between the virsh's start and taking its pid: $! is
        # it, since $! moved. Interrupted before its start, $! did not move,
        # and there is no virsh.
        RESET_VIRSH_PID=$!
    fi
    RESET_VIRSH_STARTING=false
    if [[ -n "$RESET_WATCH_PID" ]]; then
        kill "$RESET_WATCH_PID" 2>/dev/null
        for ((i = 0; i < 50; i++)); do
            child_gone "$RESET_WATCH_PID" && break
            sleep 0.1
        done
        if ! child_gone "$RESET_WATCH_PID"; then
            kill -KILL "$RESET_WATCH_PID" 2>/dev/null
        fi
        wait "$RESET_WATCH_PID" 2>/dev/null || :
        RESET_WATCH_PID=""
    fi
    [[ -n "$RESET_VIRSH_PID" ]] || return 0
    if reset_virsh_runs "$RESET_VIRSH_PID"; then
        kill "$RESET_VIRSH_PID" 2>/dev/null
        for ((i = 0; i < 50; i++)); do
            reset_virsh_runs "$RESET_VIRSH_PID" || break
            sleep 0.1
        done
        if reset_virsh_runs "$RESET_VIRSH_PID"; then
            kill -KILL "$RESET_VIRSH_PID" 2>/dev/null
            for ((i = 0; i < 50; i++)); do
                reset_virsh_runs "$RESET_VIRSH_PID" || break
                sleep 0.1
            done
        fi
    fi
    # Reaped once it has ended; never waited for while it may still run.
    if ! reset_virsh_runs "$RESET_VIRSH_PID"; then
        wait "$RESET_VIRSH_PID" 2>/dev/null || :
    fi
    RESET_VIRSH_PID=""
}

# Has the reset watch stopped? Then a reset can no longer be seen: said once,
# loudly, with what virsh last said; from here silence counts as it did
# before the watch existed (check_guard).
reset_watch_check() {
    local said
    if [[ -z "$RESET_WATCH_PID" || -n "$RESET_WATCH_DOWN" ]] || ! child_gone "$RESET_WATCH_PID"; then
        return 0
    fi
    wait "$RESET_WATCH_PID" 2>/dev/null
    RESET_WATCH_PID=""
    said="$(sed -n 's/^said //p' -- "$RESET_FILE" 2>/dev/null | tail -n 1)" || said=""
    RESET_WATCH_DOWN="libvirt's event stream ended${said:+: $said}"
    G_WATCH_DOWN_AT=$GCLOCK
    warn "THE RESET WATCH HAS STOPPED ($RESET_WATCH_DOWN): a reset of the VM can no longer be seen, so from here $(format_duration "$GUARD_SECS") without a report is taken as a boot that came without the guard"
}

# The resets the reset watch recorded ($1, its file) since the last look:
# NEW_RESETS, each as the place in the report it stood at -- or, where that
# place is not one read (the report's file was being rotated, or not there
# yet, or is gone), $2 -- and NEW_RESET_AT, when (seconds since the epoch; 0
# when not recorded: long ago, the cautious reading). A "reset" line that
# cannot be parsed is still a reset, at $2.
read_resets() {
    local rlines=() i ino size
    NEW_RESETS=() NEW_RESET_AT=()
    [[ -f "$1" ]] || return 0
    mapfile -t rlines <"$1" 2>/dev/null || return 0
    for ((i = RESET_LINES_READ; i < ${#rlines[@]}; i++)); do
        [[ "${rlines[$i]}" == reset* ]] || continue
        if [[ "${rlines[$i]}" =~ ^reset\ ([0123456789]+|-)\ ([0123456789]+)(\ ([0123456789]+))?$ ]]; then
            ino=${BASH_REMATCH[1]} size=${BASH_REMATCH[2]}
            NEW_RESET_AT+=("${BASH_REMATCH[4]:-0}")
        else
            ino=- size=0
            NEW_RESET_AT+=(0)
        fi
        if [[ "$ino" != - && -n "${R_BASES[$ino]:-}" ]]; then
            NEW_RESETS+=("$((R_BASES[$ino] + size))")
        else
            NEW_RESETS+=("$2")
        fi
    done
    RESET_LINES_READ=${#rlines[@]}
}

# The domain was started paused (afresh: --force-boot, and only once no
# managed-save image is there): read its live definition and resume it only
# if it carries the guard. Without the guard there (another define or
# session-end ran between this session's check and its start), or with no
# definition to read, it is destroyed paused -- it never ran -- and nothing
# boots. Found running instead (something else resumed it), it is never
# destroyed: an OS without its guard, it fails closed like any other.
start_guarded() {
    local out xml missing rc=0
    check_no_managed_save
    start_reset_watch
    write_guard_state "started=$(date +%s)"
    STARTED=true
    if ! out="$(virsh_ start --paused --force-boot "$DOMAIN" 2>&1)"; then
        refuse "virsh start failed: $out"
    fi
    if ! xml="$(virsh_ dumpxml "$DOMAIN" 2>&1)"; then
        missing="cannot read the started domain's definition ($xml)"
    else
        missing="$(guard_missing "$xml")"
        [[ -z "$missing" ]] || missing="the started domain does not carry the session guard (missing: $missing)"
    fi
    # From the resume on the OS may change, so a record made before it no
    # longer describes it: the time is recorded now, or nothing is resumed
    # (the domain, paused, is torn down -- it ran nothing). Not before: a
    # session that ends before this point never ran the OS, so its record
    # still describes it and a retry is admitted (bd DAS-Backup-Manager-dmxt).
    if [[ -z "$missing" ]] && ! record_session_time "$(date +%s)"; then
        missing="cannot record the start of this session ($SESSIONS_FAILURE): without it, the next session could trust a boot record made before this one"
    fi
    if [[ -n "$missing" ]]; then
        destroy_never_resumed || rc=$?
        case "$rc" in
            0) refuse "$missing -- it was destroyed while still paused, before it ran a single instruction; nothing was booted" ;;
            2 | 5)
                guard_failed "$missing, and it is $DESTROY_STATE: something other than this script resumed it" "$DESTROY_STATE"
                refuse "$missing -- it is $DESTROY_STATE: something other than this script resumed it, so it was not destroyed"
                ;;
            4) refuse "$missing -- and it was not destroyed: whether it ever ran could not be read ($DESTROY_STATE). This script never resumes it" ;;
            *) refuse "$missing -- and it could not be destroyed: it is still paused, and has run nothing (it is never resumed)" ;;
        esac
    fi
    write_guard_state "resumed=$(date +%s)"
    RESUME_ATTEMPTED=true
    VM_START=$SECONDS
    guard_watch_begin
    if ! out="$(virsh_ resume "$DOMAIN" 2>&1)"; then
        refuse "virsh resume failed: $out"
    fi
}

# ---------------------------------------------------------------------------
# Taking and giving back
# ---------------------------------------------------------------------------
take_lock() {
    local holder fd=${DAS_RECOVERY_VM_LOCK_FD:-}
    # A drive's session in a two-drive run: the run holds the lock, once,
    # and this session holds it through the run's descriptor -- which must
    # be that lock file, and held.
    if [[ -n "$fd" ]]; then
        if [[ ! "$fd" =~ ^[[:digit:]]+$ || "$(readlink -f -- "/proc/$$/fd/$fd" 2>/dev/null)" != "$(readlink -f -- "$MAINTENANCE_LOCK" 2>/dev/null)" ]]; then
            refuse "DAS_RECOVERY_VM_LOCK_FD=$fd is not an open descriptor of $MAINTENANCE_LOCK"
        fi
        if ! flock -n "$fd"; then
            refuse "DAS_RECOVERY_VM_LOCK_FD=$fd does not hold $MAINTENANCE_LOCK"
        fi
        LOCK_FD=$fd
        LOCK_INHERITED=true
        log "holding the DAS maintenance lock $MAINTENANCE_LOCK through the two-drive run that took it"
        return 0
    fi
    if ! exec {LOCK_FD}<>"$MAINTENANCE_LOCK"; then
        LOCK_FD=""
        refuse "cannot open $MAINTENANCE_LOCK"
    fi
    if ! flock -n "$LOCK_FD"; then
        holder="$(head -n 1 -- "$MAINTENANCE_LOCK" 2>/dev/null)" || holder=""
        exec {LOCK_FD}>&-
        LOCK_FD=""
        refuse "the DAS maintenance lock $MAINTENANCE_LOCK is held by: ${holder:-(no holder line)} -- try again when it is free"
    fi
    write_lock_record "$$"
    log "took the DAS maintenance lock $MAINTENANCE_LOCK: backups and scrubs now wait for this session"
}

# The lock's record: one line naming the session and the process that holds
# the lock (pid $1), as every holder of this lock writes it
# (indexer/src/maintenance.rs); a refused job prints it. Only while the lock
# is provably this session's. Display only: a record that cannot be written
# costs the name, never the lock.
write_lock_record() {
    # A two-drive run's record is the run's.
    [[ "$LOCK_INHERITED" != true ]] || return 0
    if ! printf 'recovery-os VM session %s pid %s\n' "$LABEL" "$1" >"$MAINTENANCE_LOCK"; then
        warn "could not record pid $1 as the holder of $MAINTENANCE_LOCK"
    fi
}

# Empty the lock's holder record. Only while the lock is provably this
# session's -- this script holds it, or this session's holder still runs --
# so the record of whoever takes it next is never erased.
clear_lock_record() {
    [[ "$LOCK_INHERITED" != true ]] || return 0
    if ! : >"$MAINTENANCE_LOCK"; then
        warn "could not empty the holder record in $MAINTENANCE_LOCK"
    fi
}

release_lock() {
    if [[ -n "$LOCK_FD" ]]; then
        # Emptied while still held: once released, the next holder may
        # already have written its own.
        clear_lock_record
        exec {LOCK_FD}>&-
        LOCK_FD=""
        if [[ "$LOCK_INHERITED" == true ]]; then
            log "let go of this drive's hold on the DAS maintenance lock (the two-drive run still holds it)"
        else
            log "released the DAS maintenance lock"
        fi
    fi
}

# Start the holder. 0 once it has announced its claim -- HOLDER_PID is then
# the pid its held line names; 1 with HOLDER_FAILURE set when it has not --
# then nothing of it is left running, unless it will not even stop
# (HOLDER_PID still set).
start_holder() {
    local i line="" why rc=0 started held_pid="" unit
    unit="$HOLDER_UNIT_PREFIX-$LABEL"
    HOLDER_STARTS=$((HOLDER_STARTS + 1))
    if ((HOLDER_STARTS > 1)); then
        # A replacement: the scope of the holder it replaces may linger a moment.
        unit+="-$HOLDER_STARTS"
    fi
    HOLDER_UNIT="$unit.scope"
    (umask 077 && : >"$HOLDER_OUT" && : >"$HOLDER_ERR")
    HOLDER_STARTING=true
    # A session of its own (setsid): Ctrl-C or a hangup in this terminal must
    # not end the claim. A scope of its own (systemd-run --scope, in
    # system.slice): ending the operator's login session or user manager
    # must not end it either. --scope runs the command in place -- the same
    # process, so it inherits LOCK_FD, on purpose: the lock lives as long as
    # the claim does. --expand-environment=no: the command line is used as
    # written. Started in /, so a long-lived holder pins no directory.
    (cd / && exec setsid systemd-run --scope --unit="$unit" --quiet --expand-environment=no \
        -- "$BTRDASD_BIN" recovery-os hold-disk --device "$DISK") \
        </dev/null >"$HOLDER_OUT" 2>"$HOLDER_ERR" &
    started=$!
    HOLDER_PID=$started
    HOLDER_STARTING=false
    write_record "$HOLDER_PID"
    for ((i = 0; i < HOLDER_START_TICKS; i++)); do
        if IFS= read -r line <"$HOLDER_OUT" && [[ -n "$line" ]]; then
            break
        fi
        if ! kill -0 "$started" 2>/dev/null; then
            # One more look: it may have announced just before it ended.
            IFS= read -r line <"$HOLDER_OUT" || :
            break
        fi
        sleep 0.1
    done
    # The holder is the process its line names -- with --scope the pid
    # started, but nothing is assumed: a runner that forks leaves its own.
    if [[ "$line" =~ $HELD_RE && "${BASH_REMATCH[1]}" == "$DISK" ]]; then
        held_pid=${BASH_REMATCH[2]}
    fi
    if [[ -n "$held_pid" ]] && holder_alive "$held_pid" "$DISK"; then
        HOLDER_PID=$held_pid
        write_record "$HOLDER_PID"
        record_lock_holder
        log "holding $DISK exclusively (holder pid $HOLDER_PID, $HOLDER_UNIT): nothing on this host can mount it now"
        return 0
    fi
    stop_holder "$started" "$DISK" || :
    if kill -0 "$started" 2>/dev/null; then
        HOLDER_FAILURE="it did not announce a hold within $((HOLDER_START_TICKS / 10))s and will not stop (pid $started)"
        return 1
    fi
    wait "$started" 2>/dev/null || rc=$?
    why="$(cat -- "$HOLDER_ERR" 2>/dev/null)" || why=""
    HOLDER_PID=""
    rm -f -- "$HOLDER_FILE"
    HOLDER_FAILURE="holder exit $rc: ${why:-${line:-no answer}}"
    return 1
}

# Point the lock's record at the holder: it holds the lock for as long as
# the claim lasts and outlives this script, so it is the process a reader
# must find running -- frb's readers name a holder whose recorded pid has
# gone "no longer running". Only while this script holds the lock.
record_lock_holder() {
    if [[ -n "$LOCK_FD" ]]; then
        write_lock_record "$HOLDER_PID"
    fi
}

# The claim must last as long as the VM may use the disk. A holder that died
# (killed, out of memory) is replaced at once: the VM's own open is not
# exclusive, so a new claim succeeds -- unless something on the host has
# mounted the drive meanwhile, which is then said as loudly as it deserves.
keep_claim() {
    local old=$HOLDER_PID rc=0 now
    # A USB disk that re-enumerated is another device now: the claim, and
    # the VM's own open, are on the old one, which is gone.
    now="$(readlink -f -- "$DISK" 2>/dev/null)" || now=""
    [[ -e "$DISK" ]] || now=""
    if [[ "$REENUMERATED" != true && "$now" != "$DISK_DEV" ]]; then
        REENUMERATED=true
        warn "$DISK now leads to ${now:-nothing}, not $DISK_DEV: the drive re-enumerated during the session, and the claim is on the old device. The recovery OS has most likely lost its disk; the maintenance lock still keeps this project's jobs off the drive"
    fi
    if holder_alive "$HOLDER_PID" "$DISK"; then
        return 0
    fi
    # A holder found gone is one loss; trying again after a failed claim is not.
    if [[ -n "$old" ]]; then
        wait "$old" 2>/dev/null || rc=$?
        HOLDER_LOSSES=$((HOLDER_LOSSES + 1))
        HOLDER_PID=""
        warn "the disk holder (pid $old, status $rc) is gone while the recovery OS may use $DISK -- claiming it again"
    fi
    if start_holder; then
        if [[ "$CLAIM_GAP" == true ]]; then
            warn "claimed $DISK again (holder pid $HOLDER_PID); until now nothing kept this host off it"
        fi
        CLAIM_GAP=false
        return 0
    fi
    # No holder: this process alone holds the lock now, so the record names it.
    if [[ -n "$LOCK_FD" ]]; then
        write_lock_record "$$"
    fi
    if [[ "$CLAIM_GAP" != true ]]; then
        CLAIM_GAP=true
        warn "COULD NOT CLAIM $DISK AGAIN ($HOLDER_FAILURE). If that is 'in use', something on this host has mounted the drive WHILE THE VM USES IT: unmount it at once. Until the recovery OS is off, only this script (pid $$) still holds the maintenance lock -- do not stop it. It tries again at every check."
    fi
    return 1
}

attach_disk() {
    local out
    (umask 077 && disk_xml "$DISK" >"$DISK_XML_FILE")
    # Set first: if the attach half-happens, the cleanup must look.
    ATTACHED=true
    if ! out="$(virsh_ attach-device "$DOMAIN" "$DISK_XML_FILE" --config 2>&1)"; then
        refuse "attaching $DISK to $DOMAIN failed: $out"
    fi
    if ! disk_in_definition "$DISK"; then
        refuse "virsh attached $DISK, but $DOMAIN's definition does not list it"
    fi
    log "attached $DISK to $DOMAIN: SATA disk sda, boot order 1, cache=none"
}

# Give the disk back: detach, stop the holder, rescan, check, release. Each
# step only once the one before it is proven. Returns 1, with
# RETURN_FAILURE set, leaving the claim and the lock in place, when the disk
# cannot be proven out of the domain or the holder will not stop.
return_disk() {
    local out rc=0 said cleared=false
    if [[ "$ATTACHED" == true ]]; then
        if ! out="$(virsh_ detach-disk "$DOMAIN" "$DISK" --config 2>&1)"; then
            warn "virsh detach-disk: $out"
        fi
        # The definition decides, not virsh's exit status.
        if disk_in_definition "$DISK"; then
            RETURN_FAILURE="$DISK is still in $DOMAIN's definition after detaching it"
            return 1
        fi
        ATTACHED=false
        log "detached $DISK from $DOMAIN"
    fi
    # The disk is out, so the definition can be the template's again. While
    # the claim and the lock still hold: no other session defines meanwhile.
    if [[ "$GUARDED" == true ]]; then
        remove_guard || :
    fi
    if [[ -n "$HOLDER_PID" ]]; then
        # session-end: this script holds no lock of its own, but a live
        # holder holds this session's, so the record in it is this session's.
        # Emptied before the holder goes: once it has, the lock may be
        # someone else's.
        if [[ -z "$LOCK_FD" ]] && holder_alive "$HOLDER_PID" "$DISK"; then
            clear_lock_record
            cleared=true
        fi
        if ! stop_holder "$HOLDER_PID" "$DISK"; then
            # It stays, and so does the lock it holds: name it again.
            if [[ "$cleared" == true ]]; then
                write_lock_record "$HOLDER_PID"
            fi
            RETURN_FAILURE="the disk holder (pid $HOLDER_PID) did not exit within $((HOLDER_STOP_TICKS / 10))s of SIGTERM"
            return 1
        fi
        # Its exit status, when this shell started it (127 when it did not).
        wait "$HOLDER_PID" 2>/dev/null || rc=$?
        said="$(cat -- "$HOLDER_ERR" 2>/dev/null)" || said=""
        if [[ "$HOLDER_SIGNALLED" != true ]]; then
            warn "the disk holder (pid $HOLDER_PID) had already exited${said:+: $said}"
            HOLDER_RESULT="the holder had already exited"
        elif ((rc == 0 || rc == 127)); then
            log "stopped the disk holder (pid $HOLDER_PID)${said:+: $said}"
            HOLDER_RESULT="holder stopped"
        else
            warn "the disk holder (pid $HOLDER_PID) exited with status $rc${said:+: $said}"
            HOLDER_RESULT="holder stopped (exit status $rc)"
        fi
        HOLDER_PID=""
    fi
    if [[ "$STARTED" == true ]]; then
        rescan_disk
        check_left_unmounted
    fi
    # Only an OS that was resumed can have changed (bd DAS-Backup-Manager-dmxt):
    # one started paused and torn down, or never started, ran nothing, and its
    # record still describes it.
    if [[ "$RESUME_ATTEMPTED" == true ]]; then
        if [[ -z "$SESSIONS_FILE" ]]; then
            warn "no place to record the end of this session (the boot record was never resolved)"
        elif ! record_session_time "$(date +%s)"; then
            warn "could not record the end of this session ($SESSIONS_FAILURE); its start is recorded, so the next session waits for a new boot record all the same"
        fi
    fi
    stop_console_bridge
    if [[ "$EGRESS_OWNED" == true ]]; then
        egress_remove
    fi
    release_lock
    check_lock_let_go
    rm -f -- "$HOLDER_FILE" "$HOLDER_OUT" "$HOLDER_ERR" "$DISK_XML_FILE"
    return 0
}

# After a holder start that failed, whatever that start left running has the
# lock's descriptor and would hold the lock with it -- and backups and scrubs
# would wait for a process nobody knows about. So once the lock is let go,
# look: a job that names itself in the record is a legitimate taker (given a
# moment to write it); anything else is said as loudly as it deserves.
check_lock_let_go() {
    local i first
    if [[ -z "$HOLDER_FAILURE" || -n "$LOCK_FD" || "$LOCK_INHERITED" == true ]]; then
        return 0
    fi
    for ((i = 0; i < 10; i++)); do
        if (flock -n 9) 9<"$MAINTENANCE_LOCK"; then
            return 0
        fi
        first="$(head -n 1 -- "$MAINTENANCE_LOCK" 2>/dev/null)" || first=""
        if [[ -n "$first" && "$first" != "recovery-os VM session $LABEL pid "* ]]; then
            log "the DAS maintenance lock was taken at once by: $first"
            return 0
        fi
        sleep 0.1
    done
    warn "THE MAINTENANCE LOCK IS STILL HELD after this session let it go, by no job that names itself: something left behind by the failed holder start may hold it, and backups and scrubs will wait for it. See who has it open: fuser -v $MAINTENANCE_LOCK"
}

# Another kernel wrote this filesystem: have the host's btrfs read it anew.
rescan_disk() {
    local part out
    part="$(partition_path "$DISK" 2)"
    if [[ ! -e "$part" ]]; then
        warn "$part does not exist -- no btrfs device scan"
        SCAN_RESULT="not done: $part does not exist"
        return 0
    fi
    if out="$(btrfs device scan "$part" 2>&1)"; then
        log "btrfs device scan $part: ${out:-done}"
        SCAN_RESULT="done ($part)"
    else
        warn "btrfs device scan $part failed: $out"
        SCAN_RESULT="FAILED: $out"
    fi
}

# One device's mount state as the summary says it. Prints only that: the
# warnings go to stderr.
mount_state_of() {
    local mounted
    if ! mounted="$(mounted_partitions "$1")"; then
        warn "cannot list the mounts of $1"
        printf 'unknown: lsblk failed\n'
    elif [[ -n "$mounted" ]]; then
        warn "after the session $1 is mounted: $mounted"
        printf 'MOUNTED: %s\n' "$mounted"
    else
        printf 'no partition mounted\n'
    fi
}

check_left_unmounted() {
    local now
    if [[ "$REENUMERATED" != true ]]; then
        MOUNT_RESULT="$(mount_state_of "$DISK_DEV")"
        return 0
    fi
    # The drive came back under another name, and its old one may be another
    # drive's by now: look at both, and say which is which.
    MOUNT_RESULT="$(mount_state_of "$DISK_DEV") (old device $DISK_DEV)"
    now="$(readlink -f -- "$DISK" 2>/dev/null)" || now=""
    [[ -e "$DISK" ]] || now=""
    if [[ -z "$now" ]]; then
        MOUNT_RESULT+="; $DISK leads nowhere now"
    elif [[ "$now" != "$DISK_DEV" ]]; then
        MOUNT_RESULT+="; $(mount_state_of "$now") (current device $now)"
    fi
}

# The one place a recovery OS that may have run is offered a `virsh destroy`
# (bd DAS-Backup-Manager-0zm, the operator's ruling): the facts, the
# trade-off, and the command as the operator's own choice -- never on a clock,
# never by this script. $1: what running on means here.
force_off_choice() {
    local asked=""
    if ((SHUTDOWN_ASKED > 0)); then
        asked="It was asked to shut down $(count_of "$SHUTDOWN_ASKED" time) over $(format_duration "$SHUTDOWN_SECS") (ACPI), and has not gone. "
    fi
    cat >&2 <<EOF
     ${asked}Forcing it off is your choice to make, and this script never
     makes it: on one side, $1; on the other, an update in it cut short
     (whatever it was writing, half written). If you choose to:
       virsh --connect $LIBVIRT_URI destroy $DOMAIN
EOF
}

# The session is left running ($1 why, $2 the domain's state): what the
# guard is, first, then what keeps the disk, then how to finish.
keep_session() {
    local never_ran=false paused_tried=false last="" how
    # Paused, never resumed by this script, and not found resumed by anything
    # else: it has run nothing.
    if [[ "$RESUME_ATTEMPTED" == false && "$MAY_HAVE_RUN" != true && "$2" == paused ]]; then
        never_ran=true
    elif [[ "$2" == paused ]]; then
        paused_tried=true
    fi
    warn "leaving the session in place: $1"
    # What is left must be a live holder: it is what keeps both the claim and
    # the lock once this script has exited.
    if ! keep_claim; then
        warn "ONCE THIS SCRIPT EXITS NOTHING KEEPS THE HOST OFF $DISK: end the recovery OS now (see below), then run $SELF session-end $LABEL"
    fi
    # What the guard is, first: whether this OS may run btrbk decides what
    # the operator does next.
    if [[ "$never_ran" == true ]]; then
        warn "$DOMAIN was started paused and never resumed: it has not run a single instruction, so nothing -- guarded or not -- has booted"
    elif [[ "$GUARD_FAILED" == true ]]; then
        warn "THE RECOVERY OS IS RUNNING WITHOUT A CONFIRMED SESSION GUARD (${GUARD_RESULT#NOT confirmed: }): btrbk may run in it, against the backups on $DISK"
    elif [[ "$PROOF_WHY" == reset ]]; then
        # The live rule's own answer: a boot after a reset has to prove itself,
        # and this one has not yet -- whatever the boots before it said.
        warn "THE BOOT AFTER THE RESET HAS NOT BEEN JUDGED: the recovery OS was reset, and no boot after that has reported its guard engaged yet (it has until $(format_duration "$GUARD_SECS") after the reset, not counting time paused). Judge it first: $SELF status -- it reads the resets this script saw -- and until it says engaged, treat the recovery OS as unguarded"
    elif [[ "$GUARD_CONFIRMED" == true ]]; then
        if ((G_LAST_LINE <= GCLOCK)); then
            last="; its last report $(format_duration $((GCLOCK - G_LAST_LINE))) ago, it reports every minute"
        fi
        log "the session guard is engaged ($GUARD_EXPECT$last): btrbk cannot run in the recovery OS. From here nothing watches it but $SELF status, which judges its report again -- NOT engaged means a boot came without it -- and which also counts $(format_duration "$GUARD_SECS") of silence as NOT confirmed, since it cannot see a reset made after this script ended: it cannot tell a reporter that stopped from a boot that came without the guard"
        if [[ "$SILENT" == true ]]; then
            warn "$(silence_text $((GCLOCK - G_LAST_LINE)))"
        fi
    else
        warn "THE SESSION GUARD HAS NOT BEEN JUDGED: its report lands in $GUARD_FILE. Judge it first: $SELF status -- until it says engaged, treat the recovery OS as unguarded"
    fi
    cat >&2 <<EOF
The recovery OS keeps $DISK:
  - the disk holder (pid ${HOLDER_PID:-unknown}) still claims it, so nothing on
    this host can mount any partition of it;
  - it still holds $MAINTENANCE_LOCK, so backups and scrubs wait
    and the other jobs that mount targets defer.
Never 'systemctl stop' ${HOLDER_UNIT:-the scope of the holder}: that ends the claim and the lock
at once, and with this script gone nothing takes them again.
The egress rule (if one is in place) and a console bridge stay too:
session-end takes them out.
Nothing was detached and nothing was destroyed. To finish:
EOF
    if [[ "$never_ran" == true ]]; then
        cat >&2 <<EOF
  1. Check it is still paused, and that its vCPUs have never run -- only then
     has it run nothing:
       virsh --connect $LIBVIRT_URI domstats --state --vcpu $DOMAIN
     (must say state.state=3, and vcpu.N.time=0 for every vCPU)
     So: tear it down, nothing is cut short:
       virsh --connect $LIBVIRT_URI destroy $DOMAIN
     Anything else: something other than this script resumed it, and it may
     be running (or have run) without the guard -- judge it: $SELF status
  2. Then run:          $SELF session-end $LABEL
EOF
    elif [[ "$paused_tried" == true ]]; then
        how="after a resume was tried"
        if [[ "$RESUME_ATTEMPTED" != true ]]; then
            how="and its vCPUs have run: something other than this script resumed it"
        fi
        cat >&2 <<EOF
  It is paused, $how: it may have run, and a paused guest
  runs nothing -- not even a power-off asked from its console. Either:
  1. Resume it, judge the guard, and power it off from inside it:
       virsh --connect $LIBVIRT_URI resume $DOMAIN
       $SELF status
       virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN
  2. Or end it where it stands:
EOF
        force_off_choice "a paused recovery OS that may have run, holding $DISK and the lock"
        cat >&2 <<EOF
  3. Then run:          $SELF session-end $LABEL
EOF
    elif [[ "$GUARD_CONFIRMED" == true && "$GUARD_FAILED" != true && -z "$PROOF_WHY" ]]; then
        cat >&2 <<EOF
  1. Open the console:  virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN
     Let the update finish, then power the recovery OS off from inside it.
  2. Then run:          $SELF session-end $LABEL
  Only if it has hung:
EOF
        force_off_choice "a hung recovery OS that keeps $DISK and the lock"
    else
        if [[ "$GUARD_FAILED" != true ]]; then
            cat >&2 <<EOF
  0. Judge the guard first:  $SELF status
     engaged: let the update finish, power it off from inside, then step 3.
     Anything else: steps 1 to 3.
EOF
        fi
        cat >&2 <<EOF
  1. Power it off now: open the console (virt-viewer --connect $LIBVIRT_URI
     --attach $DOMAIN) and run systemctl poweroff there.
  2. If it does not go:
EOF
        force_off_choice "a recovery OS without a confirmed session guard, running beside the backups on $DISK"
        cat >&2 <<EOF
  3. Then run:          $SELF session-end $LABEL
     It reads the guard's report before anything is removed, and says what it found.
EOF
    fi
}

keep_after_failure() {
    warn "could not give $DISK back completely: $RETURN_FAILURE"
    if [[ -n "$HOLDER_PID" ]] && holder_alive "$HOLDER_PID" "$DISK"; then
        cat >&2 <<EOF
For safety the claim and the lock stay where they are: the disk holder
(pid $HOLDER_PID) still claims $DISK and still holds
$MAINTENANCE_LOCK. Check '$SELF status', put right what failed, then run:
  $SELF session-end $LABEL
EOF
    elif [[ -n "$LOCK_FD" ]]; then
        cat >&2 <<EOF
Nothing more is undone, but the claim is gone: no disk holder runs. This
process still holds $MAINTENANCE_LOCK, and the lock ends with it.
Do not mount $DISK. Check '$SELF status', put right what failed, then run:
  $SELF session-end $LABEL
EOF
    else
        cat >&2 <<EOF
Nothing more is undone, but the claim is gone: no disk holder runs, and with
it went this session's hold on $MAINTENANCE_LOCK. Do not mount $DISK.
Check '$SELF status', put right what failed, then run this again.
EOF
    fi
}

# ---------------------------------------------------------------------------
# The egress rule (decision 5 of 2026-10-07, bd DAS-Backup-Manager-8249)
# ---------------------------------------------------------------------------
# The VM's traffic leaves through libvirt's NAT, and so follows the host's
# default route: through a VPN exit node when one is up (Tailscale's
# `lookup 52` at priority 5270), where a full update crawled at ~0.1 MB/s on
# 2026-10-07. A policy rule above it sends the VM's subnet -- and nothing
# else -- by the main table, direct. It is put in place before the domain
# starts and taken out when the disk is given back, on every way out that
# gives it back; a session left running (exit 3) leaves it, and session-end
# takes it out. Its subnet is in EGRESS_FILE while it is this script's.

# The subnet of libvirt's network LIBVIRT_NETWORK ("192.168.122.0/24") into
# EGRESS_SUBNET; 1, with EGRESS_FAILURE set, when it cannot be read: exactly
# one IPv4 address with a netmask or prefix is required.
network_subnet() {
    local xml line addr="" bits="" n=0 mask o a b c d m i net=""
    EGRESS_FAILURE=""
    if ! xml="$(virsh_ net-dumpxml "$LIBVIRT_NETWORK" 2>&1)"; then
        EGRESS_FAILURE="cannot read libvirt's network '$LIBVIRT_NETWORK': $(printable "$xml")"
        return 1
    fi
    while IFS= read -r line; do
        [[ "$line" == *"<ip "* ]] || continue
        [[ "$line" != *"family='ipv6'"* && "$line" != *'family="ipv6"'* ]] || continue
        n=$((n + 1))
        if [[ "$line" =~ $NET_ADDR_RE ]]; then addr=${BASH_REMATCH[1]}; fi
        if [[ "$line" =~ $NET_PREFIX_RE ]]; then
            bits=${BASH_REMATCH[1]}
        elif [[ "$line" =~ $NET_MASK_RE ]]; then
            mask=${BASH_REMATCH[1]} bits=0
            IFS=. read -r a b c d <<<"$mask"
            for o in "$a" "$b" "$c" "$d"; do
                for ((i = 7; i >= 0; i--)); do
                    if (((o >> i) & 1)); then bits=$((bits + 1)); fi
                done
            done
        fi
    done <<<"$xml"
    if ((n != 1)) || [[ ! "$addr" =~ $IPV4_RE || ! "$bits" =~ ^[[:digit:]]+$ ]] || ((bits < 8 || bits > 30)); then
        EGRESS_FAILURE="libvirt's network '$LIBVIRT_NETWORK' does not have exactly one IPv4 address with a netmask ($n found; address '${addr}', prefix '${bits}')"
        return 1
    fi
    IFS=. read -r a b c d <<<"$addr"
    m=$(((0xffffffff << (32 - bits)) & 0xffffffff))
    i=$((((a << 24) | (b << 16) | (c << 8) | d) & m))
    net="$(((i >> 24) & 255)).$(((i >> 16) & 255)).$(((i >> 8) & 255)).$((i & 255))"
    EGRESS_SUBNET="$net/$bits"
}

# How many rules "from EGRESS_SUBNET lookup main" are at EGRESS_PRIORITY now
# (ip -j: never a match on text); 1, with EGRESS_FAILURE set, when that cannot
# be read.
egress_rule_count() {
    local out
    if ! out="$(ip -j rule show priority "$EGRESS_PRIORITY" 2>&1)"; then
        EGRESS_FAILURE="ip rule show failed: $(printable "$out")"
        return 1
    fi
    if ! EGRESS_COUNT="$(jq --arg src "${EGRESS_SUBNET%/*}" --argjson len "${EGRESS_SUBNET#*/}" \
        '[.[] | select(.src == $src and .srclen == $len and .table == "main")] | length' <<<"${out:-[]}" 2>&1)" ||
        [[ ! "$EGRESS_COUNT" =~ ^[[:digit:]]+$ ]]; then
        EGRESS_FAILURE="cannot read ip's rules: $(printable "$EGRESS_COUNT")"
        return 1
    fi
}

# Put the rule in place, or take over one that is there (left by a session
# that did not finish; the maintenance lock says none is running): either way
# this process takes it out again. Refuses when it cannot be proven in place.
egress_add() {
    local out
    if [[ -n "$TEST_LOOP" && -n "${DAS_RECOVERY_VM_TEST_NO_EGRESS_RULE:-}" ]]; then
        warn "TEST (DAS_RECOVERY_VM_TEST_NO_EGRESS_RULE): no egress rule -- the VM's traffic follows the host's default route"
        EGRESS_RESULT="not put in place (test)"
        return 0
    fi
    network_subnet || refuse "$EGRESS_FAILURE -- the VM's traffic cannot be routed direct; nothing was booted"
    egress_rule_count || refuse "$EGRESS_FAILURE -- nothing was booted"
    # Owned, and noted, before the add: a signal (or a refusal) between the
    # add and this would leave the rule in place with nothing to take it
    # out. egress_remove takes a rule that never came out as already gone.
    EGRESS_OWNED=true
    if ! (umask 077 && printf '%s\n' "$EGRESS_SUBNET" >"$EGRESS_FILE"); then
        warn "could not note the egress rule in $EGRESS_FILE: if this session is left running, take it out by hand: ip rule del from $EGRESS_SUBNET lookup main priority $EGRESS_PRIORITY"
    fi
    if ((EGRESS_COUNT > 0)); then
        warn "the egress rule (from $EGRESS_SUBNET lookup main priority $EGRESS_PRIORITY) is already in place, left by a session that did not finish: this session takes it over and takes it out at the end"
    else
        if ! out="$(ip rule add from "$EGRESS_SUBNET" lookup main priority "$EGRESS_PRIORITY" 2>&1)"; then
            refuse "cannot put the egress rule in place (ip rule add from $EGRESS_SUBNET lookup main priority $EGRESS_PRIORITY: $(printable "$out")) -- nothing was booted"
        fi
        egress_rule_count || refuse "$EGRESS_FAILURE -- nothing was booted"
        ((EGRESS_COUNT > 0)) || refuse "ip rule add succeeded, but no rule from $EGRESS_SUBNET is at priority $EGRESS_PRIORITY -- nothing was booted"
    fi
    EGRESS_RESULT="in place (from $EGRESS_SUBNET lookup main priority $EGRESS_PRIORITY)"
    log "egress: the VM's subnet $EGRESS_SUBNET goes direct (ip rule priority $EGRESS_PRIORITY, above a VPN exit node's)"
}

# Take the rule out, every copy of it, and prove it gone. A rule that stays
# is said (exit 5): it routes only the VMs' subnet, but it was not meant to
# outlive the session.
egress_remove() {
    local out i
    if [[ -z "$EGRESS_SUBNET" ]] && ! EGRESS_SUBNET="$(head -n 1 -- "$EGRESS_FILE" 2>/dev/null)"; then
        EGRESS_SUBNET=""
    fi
    if [[ ! "$EGRESS_SUBNET" =~ ^[[:digit:].]+/[[:digit:]]+$ ]]; then
        EGRESS_RESULT="NOT taken out: which subnet it routes is not known ($EGRESS_FILE)"
        warn "the egress rule: $EGRESS_RESULT -- see: ip rule show priority $EGRESS_PRIORITY"
        return 0
    fi
    for ((i = 0; i < 5; i++)); do
        if ! egress_rule_count; then
            break
        fi
        if ((EGRESS_COUNT == 0)); then
            EGRESS_OWNED=false
            rm -f -- "$EGRESS_FILE"
            EGRESS_RESULT="taken out"
            log "egress: took the rule for $EGRESS_SUBNET out (priority $EGRESS_PRIORITY)"
            return 0
        fi
        out="$(ip rule del from "$EGRESS_SUBNET" lookup main priority "$EGRESS_PRIORITY" 2>&1)" || EGRESS_FAILURE="ip rule del: $(printable "$out")"
    done
    EGRESS_RESULT="NOT taken out (${EGRESS_FAILURE:-it is still there}): ip rule del from $EGRESS_SUBNET lookup main priority $EGRESS_PRIORITY"
    warn "the egress rule: $EGRESS_RESULT"
}

# Whether any recovery drive's domain other than this one ($DOMAIN) is not
# shut off -- it may still need the rule. Unreadable is "yes".
other_domain_running() {
    local label state
    for label in "${TARGET_LABELS[@]}"; do
        [[ "${T_ROLE[$label]}" == mirror && "$DOMAIN_BASE-$label" != "$DOMAIN" ]] || continue
        # Not defined: nothing of it runs.
        virsh_ dominfo "$DOMAIN_BASE-$label" >/dev/null 2>&1 || continue
        state="$(virsh_ domstate "$DOMAIN_BASE-$label" 2>&1)" || return 0
        [[ "$state" == "shut off" ]] || return 0
    done
    return 1
}

# The rule, as status says it.
egress_report() {
    local subnet="" rules count
    subnet="$(head -n 1 -- "$EGRESS_FILE" 2>/dev/null)" || subnet=""
    if [[ -z "$subnet" ]]; then
        printf 'none noted\n'
        return 0
    fi
    # ip failing is "unknown", never 0: a count it did not read is no count.
    if rules="$(ip rule show priority "$EGRESS_PRIORITY" 2>&1)"; then
        count="$(grep -cF "from $subnet lookup main" <<<"$rules")" || :
    else
        count="unknown (ip rule show failed: $(printable "$rules"))"
    fi
    printf 'noted for %s (priority %s); in place now: %s\n' "$subnet" "$EGRESS_PRIORITY" "$count"
}

# ---------------------------------------------------------------------------
# The console bridge (decision 2 of 2026-10-04, bd DAS-Backup-Manager-8249)
# ---------------------------------------------------------------------------
# For a running session only: the domain's VNC, which libvirt keeps on a
# root-only socket, offered through a socket of its own that only one user
# can open (0600, that user's, in a fresh 0711 directory of root's), for
# one connection: socat accepts once and ends when it closes. Removed when the
# disk is given back. No libvirt group, no ACL is changed.

# End the bridge this drive's record names, and remove its directory.
stop_console_bridge() {
    local pid="" dir="" i
    [[ -n "$CONSOLE_FILE" && -e "$CONSOLE_FILE" ]] || return 0
    { IFS= read -r pid && IFS= read -r dir; } <"$CONSOLE_FILE" 2>/dev/null || :
    if [[ "$pid" =~ ^[[:digit:]]+$ && -n "$dir" ]] &&
        [[ "$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline")" == *"socat"*"$dir/"* ]]; then
        kill "$pid" 2>/dev/null || :
        for ((i = 0; i < 50; i++)); do
            [[ -d "/proc/$pid" ]] || break
            sleep 0.1
        done
        kill -KILL "$pid" 2>/dev/null || :
    fi
    if [[ "$dir" == "$STATE_DIR/console-$LABEL-"* && "$dir" != *..* ]]; then
        rm -rf -- "$dir" "$dir.err"
    fi
    rm -f -- "$CONSOLE_FILE"
    log "console bridge for $LABEL removed"
}

cmd_console_socket() {
    local uid=$2 pw gid state xml vnc="" line dir sock pid i st dst
    require_root
    command -v socat >/dev/null || refuse "socat is not installed: the console bridge is a socat between two sockets (Arch: pacman -S socat)"
    if [[ ! "$uid" =~ ^[[:digit:]]+$ ]]; then
        refuse "'$uid' is not a numeric user id"
    fi
    pw="$(getent passwd "$uid")" || refuse "no user has the id $uid"
    IFS=: read -r _ _ _ gid _ <<<"$pw"
    load_targets
    resolve_label "$1"
    LOG_TO_STDERR=true
    if [[ ! -e "$HOLDER_FILE" ]] || ! read_record "$HOLDER_FILE" || ! holder_alive "$REC_PID" "$REC_DEV"; then
        refuse "no session of $LABEL is running (no live disk holder): a console bridge is only for a running session"
    fi
    state="$(current_state)"
    [[ "$state" == running ]] || refuse "$DOMAIN is $state -- a console bridge needs it running"
    xml="$(virsh_ dumpxml "$DOMAIN" 2>&1)" || refuse "cannot read $DOMAIN's live definition: $(printable "$xml")"
    while IFS= read -r line; do
        if [[ "$line" == *"<graphics type='vnc'"* && "$line" =~ $VNC_SOCKET_RE ]]; then
            vnc=${BASH_REMATCH[1]}
        fi
    done <<<"$xml"
    [[ -n "$vnc" ]] || refuse "$DOMAIN's live definition names no VNC socket"
    check_path_chars "$vnc"
    [[ -S "$TEST_ROOT$vnc" ]] || refuse "$vnc is not a socket"
    # One bridge per drive: a new one replaces the old.
    stop_console_bridge
    make_state_dir || refuse "cannot create $STATE_DIR"
    dir="$(mktemp -d -- "$STATE_DIR/console-$LABEL-XXXXXXXXXXXX")" || refuse "cannot create a directory for the bridge under $STATE_DIR"
    # The directory stays root's (0711: passed through, not listed, written
    # by root only); only the socket is the user's. A directory of the user's
    # would let them put a symlink where socat applies user= and mode= by
    # path, and have root chown whatever it names.
    if ! chmod 0711 -- "$dir"; then
        rm -rf -- "$dir"
        refuse "cannot set the mode of $dir"
    fi
    sock="$dir/vnc.sock"
    # Its own session, out of this command's: the caller reads this command's
    # output to its end, which must not wait for the bridge.
    setsid socat "UNIX-LISTEN:$sock,mode=600,user=$uid,group=$gid,unlink-early" "UNIX-CONNECT:$TEST_ROOT$vnc" \
        </dev/null >/dev/null 2>"$dir.err" &
    pid=$!
    if ! (umask 077 && printf '%s\n%s\n' "$pid" "$dir" >"$CONSOLE_FILE"); then
        kill "$pid" 2>/dev/null || :
        rm -rf -- "$dir" "$dir.err"
        refuse "cannot record the bridge in $CONSOLE_FILE"
    fi
    for ((i = 0; i < 50; i++)); do
        [[ -S "$sock" ]] && break
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
    done
    # stat without -L: the socket itself (a symlink is "symbolic link"), and
    # its directory still root's alone.
    st="$(stat -c '%u %a %F' -- "$sock" 2>/dev/null)" || st=""
    dst="$(stat -c '%u %a %F' -- "$dir" 2>/dev/null)" || dst=""
    if [[ "$dst" != "$EUID 711 directory" ]]; then
        stop_console_bridge
        refuse "the bridge's directory $dir is not $EUID's, mode 0711 (${dst:-not there})"
    fi
    if [[ "$st" != "$uid 600 socket" ]]; then
        stop_console_bridge
        refuse "the bridge's socket is not a socket of user $uid with mode 0600 (${st:-not there}): $(head -c 300 -- "$dir.err" 2>/dev/null)"
    fi
    rm -f -- "$dir.err"
    log "console bridge for $LABEL: $sock (user $uid, one connection; removed when the disk is given back)"
    printf '%s\n' "$sock"
}

# ---------------------------------------------------------------------------
# Unattended sessions (bd DAS-Backup-Manager-8249, decisions 1 and 3-8)
# ---------------------------------------------------------------------------
# The update an operator runs at the console, run instead through the
# recovery OS's own QEMU guest agent -- virsh qemu-agent-command, guest-exec:
# no SSH, no password. Only for a drive whose record says the agent is
# installed and starts at boot, and never on a "will" record. Each step runs
# in the recovery OS as a transient systemd unit of its own, so a restart of
# the agent (pacman upgrading it) or of systemd does not end it; this script
# looks at its exit status and output every POLL_SECS, and between two looks
# watches the claim and the guard as an attended session does. Anything
# unexpected stops the update: the recovery OS is asked to power off
# (through its agent, and by ACPI until it has; never destroyed), the disk is
# given back, and the session exits 7 naming the step.

# One progress line (the grammar is in the header): $1 the step, $2 the
# event, the rest a message.
progress() {
    local stage=$1 event=$2
    shift 2
    printf 'PROGRESS %s %s %s%s\n' "$LABEL" "$stage" "$event" "${*:+ $*}"
    logger -t "$LOG_TAG" -- "PROGRESS $LABEL $stage $event${*:+ $*}" || :
}

# A command to the guest agent ($1, JSON), answered within $2 seconds:
# AGENT_OUT is the answer, or what virsh said.
agent_call() {
    AGENT_OUT="$(virsh_ qemu-agent-command "$DOMAIN" --timeout "${2:-30}" "$1" 2>&1)"
}

# Run $2 (sh) in the recovery OS through the agent and wait for it, $1
# seconds at most: 0 once it has ended (GSH_RC its exit status, GSH_OUT what
# it printed, stdout then stderr); 1 when the agent did not run it or did not
# say it ended (GSH_WHY says why).
guest_sh() {
    local json pid start=$SECONDS exited rc out err
    GSH_RC="" GSH_OUT="" GSH_WHY=""
    json="$(jq -cn --arg s "$2" '{execute: "guest-exec", arguments: {path: "/bin/sh", arg: ["-c", $s], "capture-output": true}}')" || {
        GSH_WHY="cannot encode the command"
        return 1
    }
    if ! agent_call "$json" 30; then
        GSH_WHY="the guest agent did not take the command: $(printable "$AGENT_OUT")"
        return 1
    fi
    pid="$(jq -r '.return.pid // empty' <<<"$AGENT_OUT" 2>/dev/null)" || pid=""
    if [[ ! "$pid" =~ ^[[:digit:]]+$ ]]; then
        GSH_WHY="the guest agent gave no process for the command: $(printable "$AGENT_OUT")"
        return 1
    fi
    while :; do
        if agent_call "{\"execute\":\"guest-exec-status\",\"arguments\":{\"pid\":$pid}}" 30; then
            exited="$(jq -r '.return.exited' <<<"$AGENT_OUT" 2>/dev/null)" || exited=""
            if [[ "$exited" == true ]]; then
                rc="$(jq -r '.return.exitcode // .return.signal // empty | tostring' <<<"$AGENT_OUT" 2>/dev/null)" || rc=""
                out="$(jq -r '.return["out-data"] // empty' <<<"$AGENT_OUT" 2>/dev/null)" || out=""
                err="$(jq -r '.return["err-data"] // empty' <<<"$AGENT_OUT" 2>/dev/null)" || err=""
                GSH_OUT="$(printf '%s' "$out" | base64 -d 2>/dev/null)$(printf '%s' "$err" | base64 -d 2>/dev/null)" || :
                # An end without an exit status (a signal) is never 0.
                GSH_RC=${rc:-signalled}
                [[ "$GSH_RC" =~ ^[[:digit:]]+$ ]] || GSH_RC="ended by signal ${GSH_RC}"
                return 0
            fi
        else
            GSH_WHY="the guest agent did not answer: $(printable "$AGENT_OUT")"
        fi
        if ((SECONDS - start >= $1)); then
            GSH_WHY="no end within $(format_duration "$1")${GSH_WHY:+ ($GSH_WHY)}"
            return 1
        fi
        sleep 0.5
    done
}

# One look while an unattended step runs or waits: the domain's state, the
# claim, the guard, the --timeout. 1, with UNATTENDED_WHY set, when the update
# cannot go on.
watch_tick() {
    local state
    state="$(current_state)"
    if [[ "$state" == "shut off" ]]; then
        UNATTENDED_WHY="the recovery OS powered off by itself"
        return 1
    fi
    keep_claim || :
    check_guard running "$state"
    if [[ "$GUARD_FAILED" == true ]]; then
        UNATTENDED_WHY="the session guard did not confirm (${GUARD_RESULT#NOT confirmed: })"
        return 1
    fi
    if ((UNATTENDED_DEADLINE > 0 && SECONDS >= UNATTENDED_DEADLINE)); then
        UNATTENDED_WHY="the --timeout of $TIMEOUT_MIN minute(s) has passed"
        return 1
    fi
}

stage_failed() {
    progress "$1" fail "$2"
    UNATTENDED_STAGE=$1
    UNATTENDED_FAILED="$1: $2"
    warn "the unattended update stopped at '$1': $2"
    # A step that changes the OS (pacman, mkinitcpio) stopped for any reason
    # may still be running when the power-off ends it: mid-transaction.
    case "$1" in
        keyrings | upgrade | packages | initramfs)
            UNATTENDED_HALF="the recovery OS may be half-upgraded ('$1' may have been ended by the power-off in the middle of its work): roll back to ${PRE_UPDATE_SNAPSHOT:-the @.pre-update.<YYYYMMDD-HHMM> snapshot this run took} (read-only, at its filesystem's top level), from inside that OS or a live system, before trusting it"
            warn "$UNATTENDED_HALF"
            ;;
    esac
}

# The lines of what a step printed, as they arrive ($1 the step, $2 the new
# text): OUTPUT lines, an unfinished last line kept for the next.
stage_output() {
    local data=$OUT_PART$2 line
    while [[ "$data" == *$'\n'* ]]; do
        line=${data%%$'\n'*}
        data=${data#*$'\n'}
        line="$(printable "${line//$'\r'/}")"
        printf 'OUTPUT %s %s %s\n' "$LABEL" "$1" "$line"
        logger -t "$LOG_TAG" -- "OUTPUT $LABEL $1 $line" || :
        [[ -z "${line//[[:space:]]/}" ]] || OUT_LAST=$line
        if [[ "$1" == snapshot && "$line" =~ ^snapshot:\ (@\.pre-update\.[[:digit:]]{8}-[[:digit:]]{4})\ \(read-only\)$ ]]; then
            PRE_UPDATE_SNAPSHOT=${BASH_REMATCH[1]}
        fi
    done
    OUT_PART=$data
}

# Run step $1 in the recovery OS, $2 seconds at most, as the sh script $3:
# a transient unit of its own (it outlives a restart of the agent), its
# output and exit status in /run/das-vm-update. 0 when it ended with 0.
run_stage() {
    local stage=$1 limit=$2 n unit dir=/run/das-vm-update b64 runb64 start=$SECONDS last_ok=$SECONDS off=0 rc chunk bytes tmp
    STAGE_N=$((STAGE_N + 1))
    n=$STAGE_N
    unit="das-vm-update-$n-$stage"
    OUT_PART="" OUT_LAST=""
    progress "$stage" start
    b64="$(printf '%s\n' "$3" | base64 -w0)" || b64=""
    runb64="$(printf 'sh %s/%s.sh >%s/%s.out 2>&1; echo $? >%s/%s.rc.new && mv -f %s/%s.rc.new %s/%s.rc\n' \
        "$dir" "$n" "$dir" "$n" "$dir" "$n" "$dir" "$n" "$dir" "$n" | base64 -w0)" || runb64=""
    if [[ -z "$b64" || -z "$runb64" ]]; then
        stage_failed "$stage" "cannot encode it"
        return 1
    fi
    if ! guest_sh 60 "set -e; mkdir -p $dir; printf '%s' '$b64' | base64 -d >$dir/$n.sh; printf '%s' '$runb64' | base64 -d >$dir/$n.run; rm -f $dir/$n.rc $dir/$n.out; systemd-run --quiet --collect --no-block --unit=$unit --property=Type=oneshot /bin/sh $dir/$n.run"; then
        stage_failed "$stage" "it could not be started in the recovery OS: $GSH_WHY"
        return 1
    fi
    if [[ "$GSH_RC" != 0 ]]; then
        stage_failed "$stage" "it could not be started in the recovery OS (exit status $GSH_RC: $(printable "$GSH_OUT"))"
        return 1
    fi
    tmp="$STATE_DIR/$LABEL.stage.chunk"
    while :; do
        sleep "$POLL_SECS"
        if ! watch_tick; then
            stage_failed "$stage" "$UNATTENDED_WHY"
            return 1
        fi
        if guest_sh 30 "r=; if [ -e $dir/$n.rc ]; then r=\$(cat $dir/$n.rc); fi; printf 'rc=%s\\n' \"\$r\"; tail -c +$((off + 1)) $dir/$n.out 2>/dev/null | head -c 65536 | base64 -w0; echo" &&
            [[ "$GSH_RC" == 0 && "$GSH_OUT" == rc=* ]]; then
            last_ok=$SECONDS
            rc=${GSH_OUT%%$'\n'*}
            rc=${rc#rc=}
            # Command substitution took the trailing newline: with no output
            # yet, the answer is its first line alone.
            chunk=""
            if [[ "$GSH_OUT" == *$'\n'* ]]; then
                chunk=${GSH_OUT#*$'\n'}
                chunk=${chunk//[[:space:]]/}
            fi
            if [[ -n "$chunk" ]]; then
                if ! printf '%s' "$chunk" | base64 -d >"$tmp" 2>/dev/null; then
                    stage_failed "$stage" "its output came back unreadable"
                    rm -f -- "$tmp"
                    return 1
                fi
                bytes=$(wc -c <"$tmp")
                off=$((off + bytes))
                stage_output "$stage" "$(cat -- "$tmp"; printf x)"
                OUT_PART=${OUT_PART%x}
                rm -f -- "$tmp"
            elif [[ -n "$rc" ]]; then
                # Ended, and all it printed has been read.
                [[ -z "$OUT_PART" ]] || stage_output "$stage" $'\n'
                if [[ "$rc" == 0 ]]; then
                    progress "$stage" ok "${OUT_LAST:+$OUT_LAST}"
                    return 0
                fi
                stage_failed "$stage" "exit status $(printable "$rc")${OUT_LAST:+ -- $OUT_LAST}"
                return 1
            fi
        elif ((SECONDS - last_ok >= AGENT_GRACE_SECS)); then
            stage_failed "$stage" "the guest agent has not answered for $(format_duration $((SECONDS - last_ok))): ${GSH_WHY:-$(printable "$GSH_OUT")}"
            return 1
        fi
        if ((SECONDS - start >= limit)); then
            stage_failed "$stage" "not done within $(format_duration "$limit") (it may still run in the recovery OS)"
            return 1
        fi
    done
}

# The recovery OS answers through its agent, and its guard has reported
# engaged with nothing left to prove: within $1 seconds.
wait_guest_ready() {
    local start=$SECONDS
    while :; do
        sleep "$POLL_SECS"
        if ! watch_tick; then
            stage_failed boot "$UNATTENDED_WHY"
            return 1
        fi
        if [[ "$GUARD_CONFIRMED" == true && -z "$PROOF_WHY" ]] && agent_call '{"execute":"guest-ping"}' 10; then
            return 0
        fi
        if ((SECONDS - start >= $1)); then
            if [[ "$GUARD_CONFIRMED" == true ]]; then
                stage_failed boot "the guest agent did not answer within $(format_duration "$1") of the start (its record says it starts at boot): $(printable "$AGENT_OUT")"
            else
                stage_failed boot "the session guard did not report within $(format_duration "$1")"
            fi
            return 1
        fi
    done
}

# Reboot the recovery OS from inside, and wait for a new boot (another boot
# id) whose guard reports engaged, answering the reset, and whose agent
# answers.
reboot_guest() {
    local before now resets=$RESETS boots=$J_BOOTS start=$SECONDS
    progress reboot start
    if ! guest_sh 30 'cat /proc/sys/kernel/random/boot_id' || [[ "$GSH_RC" != 0 ]]; then
        stage_failed reboot "cannot read the boot id: ${GSH_WHY:-$(printable "$GSH_OUT")}"
        return 1
    fi
    before=${GSH_OUT//[[:space:]]/}
    agent_call "$(jq -cn '{execute: "guest-exec", arguments: {path: "/usr/bin/systemctl", arg: ["reboot"]}}')" 30 || :
    while :; do
        sleep "$POLL_SECS"
        if ! watch_tick; then
            stage_failed reboot "$UNATTENDED_WHY"
            return 1
        fi
        if ((RESETS > resets && J_BOOTS > boots)) && [[ -z "$PROOF_WHY" ]] &&
            guest_sh 15 'cat /proc/sys/kernel/random/boot_id' && [[ "$GSH_RC" == 0 ]]; then
            now=${GSH_OUT//[[:space:]]/}
            if [[ -n "$now" && "$now" != "$before" ]]; then
                progress reboot ok "boot $now, its guard engaged"
                return 0
            fi
        fi
        if ((SECONDS - start >= GUARD_SECS + AGENT_GRACE_SECS)); then
            stage_failed reboot "no new boot with its guard engaged and its agent answering within $(format_duration $((GUARD_SECS + AGENT_GRACE_SECS)))"
            return 1
        fi
    done
}

# Ask the recovery OS to power off -- through its agent, and by ACPI until it
# has (request_shutdown: never destroyed, exit 3 at the bound). $1 why.
power_off_guest() {
    if [[ "$(current_state)" == "shut off" ]]; then
        return 0
    fi
    agent_call '{"execute":"guest-shutdown","arguments":{"mode":"powerdown"}}' 10 || :
    request_shutdown "$1" unattended
}

# The steps, in order: name, limit in seconds, the function that writes its
# script. The guard is lifted for pacman (it cannot replace a file something
# is mounted on) and engaged again before the reboot.
UNATTENDED_STEPS=(
    "egress 300 step_egress"
    "snapshot 900 step_snapshot"
    "guard-lift 120 step_guard_lift"
    "keyrings 1800 step_keyrings"
    "upgrade UPDATE step_upgrade"
    "packages 1800 step_packages"
    "initramfs 1800 step_initramfs"
    "verify-btrbk 300 step_verify_btrbk"
    "verify-boot 300 step_verify_boot"
    "guard-engage 120 step_guard_engage"
)

run_unattended() {
    local entry name limit fn
    UNATTENDED_DEADLINE=0
    if [[ -n "$TIMEOUT_MIN" ]]; then
        UNATTENDED_DEADLINE=$((SECONDS + TIMEOUT_MIN * MINUTE_SECS))
    fi
    progress boot start "waiting for the session guard and the guest agent"
    if ! wait_guest_ready "$AGENT_SECS"; then
        progress poweroff start "the update stopped at boot"
        power_off_guest "the unattended update stopped at boot"
        progress poweroff ok
        return 0
    fi
    progress boot ok "the session guard is engaged and the guest agent answers"
    for entry in "${UNATTENDED_STEPS[@]}"; do
        read -r name limit fn <<<"$entry"
        [[ "$limit" != UPDATE ]] || limit=$UPDATE_SECS
        if ! run_stage "$name" "$limit" "$("$fn")"; then
            progress poweroff start "the update stopped at $name"
            power_off_guest "the unattended update stopped at $name"
            progress poweroff ok
            return 0
        fi
    done
    if ! reboot_guest; then
        progress poweroff start "the update stopped at reboot"
        power_off_guest "the unattended update stopped at reboot"
        progress poweroff ok
        return 0
    fi
    if guest_sh 30 'uname -r' && [[ "$GSH_RC" == 0 ]]; then
        KERNEL="$(printable "${GSH_OUT//[[:space:]]/}")"
        progress kernel ok "$KERNEL"
    else
        stage_failed kernel "cannot read the running kernel: ${GSH_WHY:-$(printable "$GSH_OUT")}"
        progress poweroff start "the update stopped at kernel"
        power_off_guest "the unattended update stopped at kernel"
        progress poweroff ok
        return 0
    fi
    progress poweroff start "the update is done"
    power_off_guest "the unattended update is done"
    progress poweroff ok
}

# The steps' scripts: plain sh, run as root in the recovery OS. Each prints
# what it found; a nonzero exit stops the update.

step_egress() {
    printf 'want=%s\n' "$EGRESS_ORG"
    cat <<'EOF'
ip=$(curl -fsS --max-time 20 https://api.ipify.org) || { echo "cannot read this VM's public address"; exit 2; }
org=$(curl -fsS --max-time 20 "https://ipinfo.io/$ip/org") || { echo "cannot read who owns $ip"; exit 2; }
echo "egress: $ip, $org"
case "$org" in
    "$want "*) echo "egress is direct ($want)" ;;
    *) echo "egress is $org, not $want: the update would go through another path (a VPN exit node) -- refused"; exit 3 ;;
esac
EOF
}

# The OS snapshots its own @ read-only before pacman touches it, and keeps
# the newest two: written by the drive's own OS, never by the host
# (decision 6).
step_snapshot() {
    cat <<'EOF'
set -eu
src=$(findmnt -no SOURCE /)
fsroot=$(findmnt -no FSROOT /)
[ "$fsroot" = /@ ] || { echo "/ is $fsroot, not the subvolume @: not the layout this snapshots"; exit 3; }
dev=${src%%[*}
top=/run/das-vm-update/top
mkdir -p "$top"
mount -o subvolid=5 "$dev" "$top"
trap 'umount "$top"' EXIT
name="@.pre-update.$(date +%Y%m%d-%H%M)"
btrfs subvolume snapshot -r "$top/@" "$top/$name"
echo "snapshot: $name (read-only)"
ls -d "$top"/@.pre-update.* | while read -r p; do
    case "${p##*/}" in
        @.pre-update.[[:digit:]][[:digit:]][[:digit:]][[:digit:]][[:digit:]][[:digit:]][[:digit:]][[:digit:]]-[[:digit:]][[:digit:]][[:digit:]][[:digit:]]) echo "$p" ;;
    esac
done | sort | head -n -2 | while read -r old; do
    btrfs subvolume delete "$old"
    echo "removed the older snapshot ${old##*/}"
done
EOF
}

step_guard_lift() {
    cat <<'EOF'
systemctl stop das-vm-guard
if systemctl is-active --quiet das-vm-guard; then echo "das-vm-guard is still active"; exit 3; fi
echo "guard lifted for the update (its masks hold)"
EOF
}

step_keyrings() {
    printf '%s\n' 'pacman -Sy --noconfirm --needed archlinux-keyring cachyos-keyring'
}

step_upgrade() {
    printf '%s\n' 'pacman -Syu --noconfirm'
}

# The agent with --needed; the microcode images always reinstalled: on
# 2026-10-07 drive B's packages were installed and their images were not on
# its ESP (bd DAS-Backup-Manager-ac82), which --needed would have left so.
step_packages() {
    printf '%s\n' 'pacman -S --needed --noconfirm qemu-guest-agent && pacman -S --noconfirm amd-ucode intel-ucode'
}

# Every mkinitcpio preset carries the fallback image (decision 8: drive A's
# had only 'default' while its loader named the fallback), added only where
# it is missing and the preset is of the stock shape; then every image is
# built again.
step_initramfs() {
    cat <<'EOF'
set -u
found=0
for p in /etc/mkinitcpio.d/*.preset; do
    [ -e "$p" ] || break
    found=1
    if grep -q "^PRESETS=(.*'fallback'" "$p"; then echo "$p: has the fallback image"; continue; fi
    shape=stock
    grep -qx "PRESETS=('default')" "$p" || shape=other
    if grep -q '^fallback_' "$p"; then shape=other; fi
    if [ "$shape" != stock ]; then
        echo "$p: has no fallback image and is not of the shape this edits -- put it right by hand"
        exit 3
    fi
    def=$(sed -n 's/^default_image="\(.*\)\.img"$/\1/p' "$p")
    [ -n "$def" ] || { echo "$p: no default_image line to derive the fallback from"; exit 3; }
    sed -i "s/^PRESETS=('default')\$/PRESETS=('default' 'fallback')/" "$p"
    printf '\nfallback_image="%s-fallback.img"\nfallback_options="-S autodetect"\n' "$def" >>"$p"
    echo "$p: added the fallback image ($def-fallback.img)"
done
[ "$found" = 1 ] || { echo "no mkinitcpio preset in /etc/mkinitcpio.d"; exit 3; }
mkinitcpio -P
EOF
}

step_verify_btrbk() {
    cat <<'EOF'
out=$(pacman -Qkk btrbk 2>&1); rc=$?
printf '%s\n' "$out"
[ "$rc" -eq 0 ] || { echo "pacman -Qkk btrbk: exit status $rc"; exit 3; }
grep -Eq '^btrbk: [[:digit:]]+ total files, 0 altered files$' <<OUT || { echo "btrbk's files are not as packaged"; exit 3; }
$out
OUT
EOF
}

# What the drive's own loader and fstab need, read inside it (bd
# DAS-Backup-Manager-ac82): every linux/initrd/efi file each loader entry
# names is on the ESP, and every LABEL=, UUID=, PARTUUID= and PARTLABEL= of
# /etc/fstab resolves to a device.
step_verify_boot() {
    cat <<'EOF'
esp=$(bootctl --print-esp-path 2>/dev/null) || esp=/boot
bad=0
n=0
for e in "$esp"/loader/entries/*.conf; do
    [ -e "$e" ] || break
    n=$((n + 1))
    while read -r k v rest; do
        case "$k" in
            linux | initrd | efi)
                for f in $v $rest; do
                    if [ -f "$esp/${f#/}" ]; then :; else echo "$e: $k $f is not on the ESP ($esp)"; bad=1; fi
                done
                ;;
        esac
    done <"$e"
done
[ "$n" -gt 0 ] || { echo "no loader entries under $esp/loader/entries"; bad=1; }
while read -r spec mp rest; do
    case "$spec" in
        "" | \#*) ;;
        LABEL=* | UUID=* | PARTUUID=* | PARTLABEL=*)
            findfs "$spec" >/dev/null 2>&1 || { echo "/etc/fstab: $spec (for $mp) resolves to no device"; bad=1; }
            ;;
    esac
done </etc/fstab
[ "$bad" = 0 ] || exit 3
echo "loader entries: $n, every file they name on the ESP; /etc/fstab: every device it names resolves"
EOF
}

step_guard_engage() {
    printf 'paths="%s"\n' "${GUARD_BTRBK_PATHS[*]}"
    cat <<'EOF'
systemctl start das-vm-guard
systemctl is-active --quiet das-vm-guard || { echo "das-vm-guard did not start"; exit 3; }
for p in $paths; do
    [ -e "$p" ] || continue
    findmnt -rn --mountpoint "$p" >/dev/null || { echo "$p is not bound by the guard"; exit 3; }
done
echo "guard engaged again: btrbk is bound over every btrbk present"
EOF
}

# ---------------------------------------------------------------------------
# The session history (bd DAS-Backup-Manager-8249, decision 7)
# ---------------------------------------------------------------------------
# One JSON line per drive per session that took the lock, appended when it
# ends: label, start and end (seconds since the epoch), mode (single,
# sequential, parallel), unattended, outcome (clean = exit 0, warnings = 5,
# kept = 3 or 4: the recovery OS or the disk still held, failed = anything
# else), exit, overridden (--accept-boot-record-risk let something through),
# kernel (an unattended session's, after its reboot; else null) and
# stopped_at (the step an unattended update stopped at; else null). Beside
# the boot record, readable by all.

outcome_of() {
    case "$1" in
        0) printf 'clean' ;;
        5) printf 'warnings' ;;
        3 | 4) printf 'kept' ;;
        *) printf 'failed' ;;
    esac
}

# Append this session's line ($1 its exit status), once, and say its result.
write_history() {
    local line overridden=false outcome
    [[ "$HISTORY_ARMED" == true ]] || return 0
    HISTORY_ARMED=false
    outcome="$(outcome_of "$1")"
    ((${#BOOT_OVERRIDES[@]} == 0)) || overridden=true
    printf 'RESULT %s %s %s\n' "$LABEL" "$1" "$outcome"
    if ! line="$(jq -cn --arg label "$LABEL" --argjson start "$SESSION_EPOCH" --argjson end "$(date +%s)" \
        --arg mode "$RUN_MODE" --argjson unattended "$UNATTENDED" --arg outcome "$outcome" --argjson exit "$1" \
        --argjson overridden "$overridden" --arg kernel "$KERNEL" --arg stage "$UNATTENDED_STAGE" \
        '{label: $label, start: $start, end: $end, mode: $mode, unattended: $unattended, outcome: $outcome,
          exit: $exit, overridden: $overridden, kernel: (if $kernel == "" then null else $kernel end),
          stopped_at: (if $stage == "" then null else $stage end)}' 2>&1)"; then
        warn "could not write this session into the history: $line"
        return 0
    fi
    if ! (umask 022 && printf '%s\n' "$line" >>"$HISTORY_FILE"); then
        warn "could not append this session to $HISTORY_FILE: $line"
    fi
}

# Every exit of a session that took the lock goes through here.
session_exit() {
    write_history "$1"
    write_causes "$1"
    exit "$1"
}

# The history's lines, each checked to be one JSON object with a label: 1,
# with HISTORY_FAILURE set, when any is not -- a count read past a bad line
# would be a guess.
read_history() {
    local out
    HISTORY_FAILURE="" HISTORY_LINES=""
    [[ -e "$HISTORY_FILE" ]] || return 0
    if [[ -L "$HISTORY_FILE" || ! -f "$HISTORY_FILE" ]]; then
        HISTORY_FAILURE="$HISTORY_FILE is not a regular file"
        return 1
    fi
    if ! out="$(jq -cR 'fromjson | if type == "object" and (.label | type) == "string" then . else error("not a session line") end' <"$HISTORY_FILE" 2>&1)"; then
        HISTORY_FAILURE="$HISTORY_FILE cannot be read as the session history: $(printable "$out")"
        return 1
    fi
    HISTORY_LINES=$out
}

cmd_history() {
    load_targets
    resolve_os_state
    if [[ -n "${1:-}" ]]; then
        resolve_label "$1"
    fi
    read_history || refuse "$HISTORY_FAILURE"
    [[ -n "$HISTORY_LINES" ]] || return 0
    if [[ -n "$LABEL" ]]; then
        jq -c --arg l "$LABEL" 'select(.label == $l)' <<<"$HISTORY_LINES"
    else
        printf '%s\n' "$HISTORY_LINES"
    fi
}

# Consecutive clean unattended sessions of one drive, newest last: a session
# that failed, was kept, ended with warnings, or was overridden sets it to 0;
# a clean attended one leaves it as it is.
cmd_clean_runs() {
    local n
    load_targets
    resolve_os_state
    resolve_label "$1"
    read_history || refuse "$HISTORY_FAILURE"
    if [[ -z "$HISTORY_LINES" ]]; then
        printf '0\n'
        return 0
    fi
    n="$(jq -s --arg l "$LABEL" 'reduce (.[] | select(.label == $l)) as $s (0;
        if ($s.overridden != false) or ($s.outcome != "clean") then 0
        elif $s.unattended == true then . + 1
        else . end)' <<<"$HISTORY_LINES")" || refuse "cannot count $LABEL's clean runs"
    printf '%s\n' "$n"
}

# ---------------------------------------------------------------------------
# Both drives in one run (the operator's decision of 2026-10-04 11:39)
# ---------------------------------------------------------------------------
# ONE process holds the maintenance lock, once, for the whole run, and runs
# each drive's session as its child with that lock's descriptor
# (DAS_RECOVERY_VM_LOCK_FD): sequential runs the first drive to its end and
# the second only if the first exited 0, or 5 on warnings of its own session
# (pair_judge_causes) -- a bad update must not reach both
# -- and parallel runs both at once. The egress rule is this process's too,
# put in place once and taken out at the end. Each drive's lines carry its
# label; each drive's end is a DRIVE line here.

# What a drive's exit-5 cause (exit5_causes) means, for people.
cause_text() {
    case "$1" in
        guard-unconfirmed-no) printf 'the session guard did not confirm on a "no" record' ;;
        report-lines-lost) printf "the guard's reporter lost lines or left its last one unfinished" ;;
        guard-left) printf "the session guard could not be taken out of the domain's definition" ;;
        egress-not-removed) printf 'the egress rule was not taken out' ;;
        dry-run-override) printf 'a dry run with --accept-boot-record-risk' ;;
        claim-lost) printf 'the claim was lost while the VM ran' ;;
        reenumerated) printf 'the drive re-enumerated during the session' ;;
        mounted-after) printf 'a partition was mounted afterwards' ;;
        mount-unknown) printf 'whether a partition was mounted afterwards is not known' ;;
        scan-failed) printf 'the btrfs device scan failed' ;;
        clock-gap) printf 'time was skipped between two looks at the recovery OS' ;;
        reset-watch-stopped) printf 'the reset watch stopped' ;;
        boot-override) printf 'the boot-record check was overridden' ;;
        unmaskable-units) printf 'the boot record names units the guard cannot mask' ;;
        reporter-silent) printf "the guard's reporter went silent" ;;
        *) printf 'an unknown cause' ;;
    esac
}

# A sequential run's first drive exited 5: may the second start? $1 the file
# its session wrote its causes to (write_causes). Sets PAIR_STOP (why not --
# empty: it may) and PAIR_GO (the session-local warnings it went past). Only
# a cause that stayed in that session lets the run go on (the operator's
# decision 9, bd 8249); one of the host, the enclosure or the mechanism stops
# it, and so -- the cautious way -- does no cause, a record that cannot be
# read, and any cause not classified here.
pair_judge_causes() {
    local f=$1 line causes=() stop=() go=()
    PAIR_STOP="" PAIR_GO=""
    if [[ -L "$f" || ! -f "$f" ]] || ! mapfile -t causes <"$f" 2>/dev/null; then
        PAIR_STOP="its causes cannot be read ($f)"
        return 0
    fi
    for line in "${causes[@]}"; do
        case "$line" in
            guard-unconfirmed-no | report-lines-lost | guard-left | egress-not-removed | dry-run-override)
                go+=("$(cause_text "$line")") ;;
            claim-lost | reenumerated | mounted-after | mount-unknown | scan-failed | clock-gap | reset-watch-stopped)
                stop+=("$(cause_text "$line")") ;;
            "") ;;
            *[!a-z-]*) stop+=("a cause that cannot be read") ;;
            *) stop+=("an unclassified cause ($line)") ;;
        esac
    done
    if ((${#stop[@]} == 0 && ${#go[@]} == 0)); then
        PAIR_STOP="no cause was recorded"
        return 0
    fi
    local IFS=';'
    PAIR_STOP="${stop[*]}"
    PAIR_GO="${go[*]}"
    PAIR_STOP=${PAIR_STOP//;/; } PAIR_GO=${PAIR_GO//;/; }
}

# This run's status from the drives' ($@): a drive still held (4, then 3)
# first, then 6, 7, 5, 1, 0 -- the gravest that still asks something of the
# operator.
pair_status() {
    local want rc
    for want in 4 3 6 7 5 1; do
        for rc in "$@"; do
            if [[ "$rc" == "$want" ]]; then
                printf '%s\n' "$want"
                return 0
            fi
        done
    done
    for rc in "$@"; do
        if [[ "$rc" != 0 && "$rc" != skipped ]]; then
            printf '1\n'
            return 0
        fi
    done
    printf '0\n'
}

cmd_session_pair() {
    local first=$1 second=$2 labels=() l rcs=() pids=() i rc kept=false flags=() cause_file="" why=()
    require_root
    command -v jq >/dev/null || refuse "jq is not installed: every session reads its boot record with it"
    load_targets
    for l in "$first" "$second"; do
        resolve_label "$l"
        labels+=("$LABEL")
    done
    if [[ "${labels[0]}" == "${labels[1]}" ]]; then
        refuse "'$first' and '$second' are the same drive (${labels[0]})"
    fi
    LABEL="${labels[0]}+${labels[1]}"
    [[ "$DRY_RUN" != true ]] || flags+=(--dry-run)
    [[ "$UNATTENDED" != true ]] || flags+=(--unattended)
    [[ "$ACCEPT_BOOT_RISK" != true ]] || flags+=(--accept-boot-record-risk)
    [[ -z "$TIMEOUT_MIN" ]] || flags+=(--timeout "$TIMEOUT_MIN")
    make_state_dir || refuse "cannot create $STATE_DIR"
    wait_for_lock
    take_lock
    trap 'pair_on_exit' EXIT
    # An interrupt reaches the drives' sessions themselves (they are in this
    # process group); this one waits for them to end.
    trap ':' INT TERM HUP
    if [[ "$DRY_RUN" != true ]]; then
        egress_add
    fi
    log "both drives, $PAIR_MODE: ${labels[0]}, then ${labels[1]}$([[ "$PAIR_MODE" == parallel ]] && printf ' -- at once')"
    if [[ "$PAIR_MODE" == parallel ]]; then
        for l in "${labels[@]}"; do
            DAS_RECOVERY_VM_LOCK_FD=$LOCK_FD DAS_RECOVERY_VM_EGRESS_HELD=1 DAS_RECOVERY_VM_MODE=parallel \
                bash "$SELF" session "$l" "${flags[@]}" &
            pids+=($!)
        done
        for i in 0 1; do
            rc=0
            while :; do
                wait "${pids[$i]}" && rc=0 || rc=$?
                # A wait cut short by a signal to this process: wait again.
                kill -0 "${pids[$i]}" 2>/dev/null || break
            done
            rcs[i]=$rc
        done
    else
        for i in 0 1; do
            if ((i == 1)) && [[ "${rcs[0]}" == 5 ]]; then
                pair_judge_causes "$cause_file"
                if [[ -n "$PAIR_STOP" ]]; then
                    rcs[1]=skipped
                    why[1]="skipped: ${labels[0]} exited 5 -- $PAIR_STOP"
                    warn "${labels[1]} is not started: ${labels[0]} exited 5 -- $PAIR_STOP -- and a fault of the host, the enclosure or the mechanism may meet the next drive too"
                    break
                fi
                why[0]="exit 5, went on past: $PAIR_GO"
                warn "${labels[0]} exited 5 on warnings of its own session only ($PAIR_GO): ${labels[1]} is started"
            elif ((i == 1)) && [[ "${rcs[0]}" != 0 ]]; then
                rcs[1]=skipped
                why[1]="skipped: ${labels[0]} exited ${rcs[0]}"
                warn "${labels[1]} is not started: ${labels[0]} exited ${rcs[0]}, and a bad update must not reach both drives"
                break
            fi
            rc=0
            cause_file="$STATE_DIR/${labels[$i]}.exit5"
            rm -f -- "$cause_file"
            DAS_RECOVERY_VM_LOCK_FD=$LOCK_FD DAS_RECOVERY_VM_EGRESS_HELD=1 DAS_RECOVERY_VM_MODE=sequential \
                DAS_RECOVERY_VM_CAUSE_FILE=$cause_file bash "$SELF" session "${labels[$i]}" "${flags[@]}" || rc=$?
            rcs[i]=$rc
        done
        rm -f -- "$STATE_DIR/${labels[0]}.exit5" "$STATE_DIR/${labels[1]}.exit5"
    fi
    for i in 0 1; do
        printf 'DRIVE %s %s\n' "${labels[$i]}" "${rcs[$i]}"
        [[ "${rcs[$i]}" != 3 && "${rcs[$i]}" != 4 ]] || kept=true
    done
    printf 'Both drives -- %s, %s\n' "$LABEL" "$PAIR_MODE"
    for i in 0 1; do
        printf '  %-22s %s\n' "${labels[$i]}" "${why[$i]:-exit ${rcs[$i]}}"
    done
    PAIR_RCS=("${rcs[@]}")
    PAIR_LABELS=("${labels[@]}")
    PAIR_KEPT=$kept
    PAIR_DONE=true
    rc="$(pair_status "${rcs[@]}")"
    exit "$rc"
}

# The end of a two-drive run, on every way out: the egress rule out unless a
# drive's recovery OS may still run (a drive kept: session-end takes it out),
# the lock released (a kept drive's holder holds it on).
pair_on_exit() {
    local rc=$? i
    set +e
    trap '' INT TERM HUP
    if [[ "$EGRESS_OWNED" == true ]]; then
        if [[ "$PAIR_DONE" == true && "$PAIR_KEPT" != true ]]; then
            egress_remove
        else
            warn "the egress rule stays in place (from $EGRESS_SUBNET lookup main priority $EGRESS_PRIORITY): a drive's session is left in place; session-end takes it out once no recovery OS runs"
        fi
    fi
    release_lock
    # A kept drive's holder holds the lock on: the record names it, as a
    # single session's does.
    for i in "${!PAIR_LABELS[@]}"; do
        if [[ "${PAIR_RCS[$i]}" == 3 || "${PAIR_RCS[$i]}" == 4 ]] && read_record "$STATE_DIR/${PAIR_LABELS[$i]}.holder" &&
            holder_alive "$REC_PID" "$REC_DEV"; then
            LABEL=${PAIR_LABELS[$i]}
            write_lock_record "$REC_PID"
            break
        fi
    done
    if [[ "$EGRESS_RESULT" == "NOT taken out"* && "$rc" == 0 ]]; then
        rc=5
    fi
    exit "$rc"
}

# ---------------------------------------------------------------------------
# The EXIT trap of a session
# ---------------------------------------------------------------------------
on_exit() {
    local rc=$? state
    set +e
    trap '' INT TERM HUP
    # The reset watch ends with this script, on every way out.
    stop_reset_watch
    if [[ "$DONE" == true ]]; then
        exit "$rc"
    fi
    # Interrupted between starting the holder and recording its pid.
    if [[ -z "$HOLDER_PID" && "$HOLDER_STARTING" == true ]]; then
        HOLDER_PID=$!
    fi
    if [[ -z "$LOCK_FD" && -z "$HOLDER_PID" && "$ATTACHED" != true ]]; then
        # Nothing taken yet: a refusal keeps its status, a signal becomes 1.
        if ((rc > 128)); then
            rc=1
        fi
        exit "$rc"
    fi
    if [[ "$STARTED" == true ]]; then
        state="$(current_state)"
        # Started paused and never resumed (an interrupt, or a start that said
        # it failed but did start): it ran nothing, so it may be torn down --
        # destroy_never_resumed reads the state and the vCPUs' time again and
        # refuses anything not paused, or whose vCPUs ran. Paused after a
        # resume was tried, or found resumed by something else, it may have
        # run: never.
        if [[ "$state" == paused && "$RESUME_ATTEMPTED" == false && "$MAY_HAVE_RUN" != true ]]; then
            destroy_never_resumed || :
            state="$(current_state)"
        fi
        if [[ "$state" != "shut off" ]]; then
            # What reached the report since the last look is judged too: a
            # NOT engaged line in the last poll's interval is said.
            if [[ "$RESUME_ATTEMPTED" == true ]]; then
                check_guard final "$state"
            fi
            keep_session "${KEEP_REASON:-the driver stopped while the recovery OS is $state}" "$state"
            session_exit 3
        fi
        # It ran and is off: its report is judged before giving the disk back
        # takes the report away with the guard.
        if [[ "$RESUME_ATTEMPTED" == true ]]; then
            check_guard off
        fi
    fi
    if ! return_disk; then
        keep_after_failure
        session_exit 4
    fi
    # Both ways to a destroy end here (a start that did not carry the guard,
    # and a session stopped while paused): the one place that says what it
    # means.
    if [[ "$DESTROYED" == true ]]; then
        warn "session for $LABEL ended; the disk is the host's again and nothing is held. $(never_ran_text)"
        session_exit 1
    fi
    if [[ "$GUARD_FAILED" == true && "$VERDICT" != no ]]; then
        warn "session for $LABEL ended early; the disk is the host's again and nothing is held -- but the session guard did not confirm on a \"$VERDICT\" record (${GUARD_RESULT#NOT confirmed: }): look at the recovery OS's journal for what ran (journalctl -b -1 inside it, at its next boot)"
        session_exit 6
    fi
    if [[ "$RESUME_ATTEMPTED" == true ]]; then
        log "session guard: $GUARD_RESULT"
    fi
    log "session for $LABEL ended early; the disk is the host's again and nothing is held"
    # A guard that did not confirm on a "no" record: as a finished session.
    if [[ "$GUARD_FAILED" == true ]]; then
        session_exit 5
    fi
    session_exit 1
}

# ---------------------------------------------------------------------------
# session
# ---------------------------------------------------------------------------
parse_session_args() {
    local mode_given=""
    while (($#)); do
        case "$1" in
            --dry-run) DRY_RUN=true ;;
            --accept-boot-record-risk) ACCEPT_BOOT_RISK=true ;;
            --timeout)
                (($# >= 2)) || usage
                TIMEOUT_MIN=$2
                shift
                ;;
            --timeout=*) TIMEOUT_MIN=${1#*=} ;;
            --wait-lock)
                (($# >= 2)) || usage
                WAIT_LOCK_MIN=$2
                shift
                ;;
            --wait-lock=*) WAIT_LOCK_MIN=${1#*=} ;;
            --unattended) UNATTENDED=true ;;
            --mode)
                (($# >= 2)) || usage
                mode_given=$2
                shift
                ;;
            --mode=*) mode_given=${1#*=} ;;
            -*) usage ;;
            *)
                if [[ -z "$TARGET_ARG" ]]; then
                    TARGET_ARG=$1
                elif [[ -z "$TARGET_ARG2" ]]; then
                    TARGET_ARG2=$1
                else
                    usage
                fi
                ;;
        esac
        shift
    done
    [[ -n "$TARGET_ARG" ]] || usage
    if [[ -n "$mode_given" ]]; then
        if [[ -z "$TARGET_ARG2" ]]; then
            printf 'recovery-os-vm.sh: --mode is for a session of two drives (session A B --mode sequential|parallel)\n' >&2
            exit 2
        fi
        case "$mode_given" in
            sequential | parallel) PAIR_MODE=$mode_given ;;
            *)
                printf 'recovery-os-vm.sh: --mode takes sequential or parallel\n' >&2
                exit 2
                ;;
        esac
    fi
    if [[ -n "$TIMEOUT_MIN" && ! "$TIMEOUT_MIN" =~ ^[123456789][[:digit:]]*$ ]]; then
        printf 'recovery-os-vm.sh: --timeout takes a whole number of minutes, at least 1\n' >&2
        exit 2
    fi
    if [[ "$DRY_RUN" == true && -n "$TIMEOUT_MIN" ]]; then
        printf 'recovery-os-vm.sh: --timeout has no meaning with --dry-run (nothing is booted)\n' >&2
        exit 2
    fi
    if [[ -n "$WAIT_LOCK_MIN" && ! "$WAIT_LOCK_MIN" =~ ^[123456789][[:digit:]]*$ ]]; then
        printf 'recovery-os-vm.sh: --wait-lock takes a whole number of minutes, at least 1\n' >&2
        exit 2
    fi
    if [[ "$DRY_RUN" == true && -n "$WAIT_LOCK_MIN" ]]; then
        printf 'recovery-os-vm.sh: --wait-lock has no meaning with --dry-run (nothing is booted, and the lock is only tried)\n' >&2
        exit 2
    fi
    # A drive's session of a two-drive run: its mode, and what the run holds.
    case "${DAS_RECOVERY_VM_MODE:-}" in
        "") ;;
        sequential | parallel)
            if [[ -n "$TARGET_ARG2" || -z "${DAS_RECOVERY_VM_LOCK_FD:-}" ]]; then
                printf 'recovery-os-vm.sh: DAS_RECOVERY_VM_MODE is set only by a two-drive run, for one drive, with its lock\n' >&2
                exit 2
            fi
            RUN_MODE=$DAS_RECOVERY_VM_MODE
            ;;
        *)
            printf 'recovery-os-vm.sh: DAS_RECOVERY_VM_MODE takes sequential or parallel\n' >&2
            exit 2
            ;;
    esac
    if [[ -n "${DAS_RECOVERY_VM_EGRESS_HELD:-}" ]]; then
        if [[ -z "${DAS_RECOVERY_VM_LOCK_FD:-}" ]]; then
            printf 'recovery-os-vm.sh: DAS_RECOVERY_VM_EGRESS_HELD is set only by a two-drive run\n' >&2
            exit 2
        fi
        EGRESS_HELD=true
    fi
    if [[ -n "${DAS_RECOVERY_VM_CAUSE_FILE:-}" ]]; then
        if [[ "$RUN_MODE" != sequential ]]; then
            printf 'recovery-os-vm.sh: DAS_RECOVERY_VM_CAUSE_FILE is set only by a sequential two-drive run\n' >&2
            exit 2
        fi
        CAUSE_FILE=$DAS_RECOVERY_VM_CAUSE_FILE
    fi
}

# Ask the recovery OS to power off (ACPI) because of $1, again every
# RESEND_SECS -- one asked while it is in its firmware, boot menu or
# initramfs is dropped -- and wait until it has. Never destroyed: it may be in
# the middle of an update. Not off within GRACE_SECS: the session is kept
# (exit 3). $2: why -- "timeout" (the operator's --timeout: an update may
# still be running, which is said) or "guard" (it did not confirm).
request_shutdown() {
    local out deadline state next=0 start=$SECONDS
    deadline=$((SECONDS + GRACE_SECS))
    while :; do
        if ((SECONDS >= next)); then
            SHUTDOWN_ASKED=$((SHUTDOWN_ASKED + 1))
            log "$1: asking the recovery OS to shut down$( ((SHUTDOWN_ASKED > 1)) && printf ' (request %s)' "$SHUTDOWN_ASKED")"
            if ! out="$(virsh_ shutdown "$DOMAIN" 2>&1)"; then
                warn "virsh shutdown: $out"
            fi
            next=$((SECONDS + RESEND_SECS))
        fi
        sleep "$POLL_SECS"
        SHUTDOWN_SECS=$((SECONDS - start))
        state="$(current_state)"
        if [[ "$state" == "shut off" ]]; then
            log "the recovery OS shut down on request"
            return 0
        fi
        keep_claim || :
        ((SECONDS < deadline)) || break
    done
    KEEP_REASON="it did not power off within $(format_duration "$GRACE_SECS") of the shutdown request ($1)"
    if [[ "$2" == timeout ]]; then
        KEEP_REASON+=" -- it may still be updating"
    fi
    exit 3
}

wait_for_poweroff() {
    local deadline=0 last="running" state
    if [[ -n "$TIMEOUT_MIN" ]]; then
        deadline=$((SECONDS + TIMEOUT_MIN * MINUTE_SECS))
    fi
    log "waiting for the recovery OS to power off (state polled every ${POLL_SECS}s; the session guard must report within $(format_duration "$GUARD_SECS")${TIMEOUT_MIN:+; shutdown requested after $TIMEOUT_MIN minute(s)})"
    while :; do
        sleep "$POLL_SECS"
        state="$(current_state)"
        if [[ "$state" != "$last" ]]; then
            log "domain state: $last -> $state"
            last=$state
        fi
        if [[ "$state" == "shut off" ]]; then
            return 0
        fi
        keep_claim || :
        check_guard running "$state"
        if [[ "$GUARD_SHUT_DOWN" == true ]]; then
            return 0
        fi
        if ((deadline > 0 && SECONDS >= deadline)); then
            request_shutdown "the --timeout of $TIMEOUT_MIN minute(s) has passed" timeout
            return 0
        fi
    done
}

# Why this session needs a look -- the causes of its exit 5 -- one word per
# line, in the summary's order; nothing when there is none. The one list
# both session_status and a sequential two-drive run read (bd 8249, decision
# 9): the run goes on to its second drive only past causes that stayed in
# this session (pair_judge_causes), so a new one is added here AND classified
# there, or it stops the run.
exit5_causes() {
    ((HOLDER_LOSSES == 0)) || echo claim-lost
    [[ "$REENUMERATED" != true ]] || echo reenumerated
    if ((${#BOOT_OVERRIDES[@]} > 0)); then
        if [[ "$DRY_RUN" == true ]]; then echo dry-run-override; else echo boot-override; fi
    fi
    [[ "$DRY_RUN" != true ]] || return 0
    [[ "$GUARD_FAILED" != true ]] || echo guard-unconfirmed-no
    [[ -z "$GUARD_LEFT" ]] || echo guard-left
    ((${#UNMASKABLE[@]} == 0)) || echo unmaskable-units
    ((SILENCES == 0)) || echo reporter-silent
    [[ -z "$RESET_WATCH_DOWN" ]] || echo reset-watch-stopped
    ((${#REPORT_LOSSES[@]} == 0)) || echo report-lines-lost
    ((G_GAP_SECS == 0)) || echo clock-gap
    [[ "$EGRESS_RESULT" != "NOT taken out"* ]] || echo egress-not-removed
    [[ "$SCAN_RESULT" != FAILED* ]] || echo scan-failed
    # A drive that re-enumerated is looked at under both names.
    case "$MOUNT_RESULT" in
        *"MOUNTED: "*) echo mounted-after ;;
        *unknown*) echo mount-unknown ;;
        "no partition mounted"*) ;;
        *) echo mount-unknown ;;
    esac
}

# A sequential two-drive run's drive: write its exit-5 causes ($1 its exit
# status; none unless 5) where the run reads them. A file that cannot be
# written is said: the run then finds none and stops, the cautious way.
write_causes() {
    local tmp
    [[ -n "$CAUSE_FILE" ]] || return 0
    tmp="$CAUSE_FILE.tmp.$$"
    if ! { if [[ "$1" == 5 ]]; then exit5_causes; fi; } >"$tmp" 2>/dev/null || ! mv -f -- "$tmp" "$CAUSE_FILE"; then
        rm -f -- "$tmp"
        warn "could not write this session's exit-5 causes to $CAUSE_FILE: the two-drive run will not start the other drive"
    fi
}

# 6 when the session guard did not confirm on a record that does not rule
# btrbk out; 5 when a session ended, everything given back, but something
# needs a look; else 0.
session_status() {
    if [[ "$GUARD_FAILED" == true && "$VERDICT" != no ]]; then
        echo 6
    elif [[ -n "$UNATTENDED_FAILED" ]]; then
        echo 7
    elif [[ -n "$(exit5_causes)" ]]; then
        echo 5
    else
        echo 0
    fi
}

cmd_session() {
    parse_session_args "$@"
    if [[ -n "$TARGET_ARG2" ]]; then
        cmd_session_pair "$TARGET_ARG" "$TARGET_ARG2"
    fi
    require_root
    # No fallback: a holder in the login session ends with it, taking the
    # claim and the lock while the VM may still use the disk.
    if ! command -v systemd-run >/dev/null; then
        refuse "systemd-run is not available: the disk holder must run in a scope of its own, outside your login session, and there is no other way to put it there"
    fi
    if ! command -v jq >/dev/null; then
        refuse "jq is not installed: the boot-record check reads the record with it, and no session starts without that check"
    fi
    load_targets
    resolve_label "$TARGET_ARG"
    if [[ "$RUN_MODE" != single ]]; then
        LOG_PREFIX="[$LABEL] "
    fi
    if [[ ! "$LABEL" =~ ^[${ASCII_LETTERS}[:digit:]:_.-]+$ ]]; then
        refuse "the label '$LABEL' has characters a systemd unit name cannot carry"
    fi
    if [[ "$UNATTENDED" == true ]] && ! command -v base64 >/dev/null; then
        refuse "base64 is not installed: an unattended session reads the guest agent's answers with it"
    fi
    check_boot_record
    build_guard
    # Every unit the record names must be masked when it says btrbk may (or,
    # let through, will) run: a name past the limit, or one that cannot be
    # put into a credential, is a runner left free. Not for --accept-boot-
    # record-risk to let through: it is the guard, not the record, that fails.
    if ((${#UNMASKABLE[@]} > 0)) && [[ "$VERDICT" != no ]]; then
        refuse "the boot record says btrbk $VERDICT run when this OS boots, and names units the session guard cannot mask: ${UNMASKABLE[*]} -- put them right in the recovery OS on bare metal (see the disaster recovery guide), then let a backup run record it again"
    fi
    resolve_disk
    log "session for $LABEL: $DISK ($DISK_DEV)$([[ "$DRY_RUN" == true ]] && printf ' -- dry run')"
    wait_for_lock
    check_units
    check_not_mounted
    check_domain_idle
    check_no_live_holder
    make_state_dir || refuse "cannot create $STATE_DIR"

    SESSION_START=$SECONDS
    trap on_exit EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    trap 'exit 129' HUP

    take_lock
    if [[ "$DRY_RUN" != true ]]; then
        # From here every end of this session is written into the history.
        SESSION_EPOCH=$(date +%s)
        HISTORY_ARMED=true
    fi
    if [[ "$UNATTENDED" == true ]]; then
        progress preflight ok "lock taken; unattended, $RUN_MODE"
    else
        progress preflight ok "lock taken; attended, $RUN_MODE"
    fi
    if ! start_holder; then
        refuse "could not hold $DISK ($HOLDER_FAILURE)"
    fi

    if [[ "$DRY_RUN" == true ]]; then
        log "dry run: the disk that would be attached to $DOMAIN:"
        disk_xml "$DISK"
        trap '' INT TERM HUP
        if ! return_disk; then
            keep_after_failure
            DONE=true
            exit 4
        fi
        DONE=true
        log "dry run: the session guard must report within $(format_duration "$GUARD_SECS") of the start, and of every reset; silence after that is a warning"
        log "dry run done: lock taken and released, holder started and stopped, nothing defined or attached"
        if ((${#BOOT_OVERRIDES[@]} > 0)); then
            warn "dry run: the boot-record check was overridden (--accept-boot-record-risk) -- exit 5"
            write_causes 5
            exit 5
        fi
        exit 0
    fi

    define_guard
    attach_disk
    # A USB disk that re-enumerated since the claim is another device now.
    if [[ "$(readlink -f -- "$DISK")" != "$DISK_DEV" ]]; then
        refuse "$DISK no longer leads to $DISK_DEV (the drive re-enumerated) -- start again"
    fi
    if [[ "$EGRESS_HELD" != true ]]; then
        egress_add
    fi
    progress start start "booting $DOMAIN from $DISK"
    start_guarded
    progress start ok "the recovery OS is booting"
    log "the recovery OS is booting from $DISK"
    log "console: virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN (its VNC is a libvirt-private socket; '$SELF screenshot $LABEL <file.png>' works too)"
    log "serial console: virsh --connect $LIBVIRT_URI console $DOMAIN"

    if [[ "$UNATTENDED" == true ]]; then
        run_unattended
    else
        log "inside it, before updating: systemctl stop das-vm-guard; after: pacman -Qkk btrbk must find 0 altered files, then systemctl start das-vm-guard"
        progress wait start "waiting for the recovery OS to power off"
        wait_for_poweroff
    fi
    # It may have reported between two looks, or never.
    check_guard off

    log "the recovery OS powered off after $(format_duration $((SECONDS - VM_START))) -- giving $DISK back to the host"
    progress giveback start
    trap '' INT TERM HUP
    if ! return_disk; then
        keep_after_failure
        DONE=true
        session_exit 4
    fi
    DONE=true
    progress giveback ok "detached; $HOLDER_RESULT; egress rule $EGRESS_RESULT"
    local claim="held throughout" warnings=() w lines="" joined="" asked="" unattended_line=""
    if ((HOLDER_LOSSES > 0)) && [[ "$CLAIM_GAP" == true ]]; then
        claim="LOST $HOLDER_LOSSES time(s); NOT claimed again -- see the warnings above"
        warnings+=("the claim was lost and not taken again: until the recovery OS was off, nothing kept this host off the drive")
    elif ((HOLDER_LOSSES > 0)); then
        claim="LOST $HOLDER_LOSSES time(s) and claimed again -- see the warnings above"
    fi
    if [[ "$REENUMERATED" == true ]]; then
        warnings+=("the disk re-enumerated during the session; the claim was on the old device")
    fi
    for w in "${BOOT_OVERRIDES[@]}"; do
        warnings+=("the boot-record check was overridden (--accept-boot-record-risk): $w")
    done
    if [[ "$GUARD_FAILED" == true && "$VERDICT" == no ]]; then
        warnings+=("the session guard did not confirm (${GUARD_RESULT#NOT confirmed: }): only the boot record's \"no\" stood between btrbk and the backups on this drive")
    elif [[ "$GUARD_FAILED" == true ]]; then
        # Not inside the assignment: there a false test would be its status.
        if [[ "$GUARD_SHUT_DOWN" == true ]]; then
            asked=", so the recovery OS was asked to shut down"
        fi
        warnings+=("the session guard did not confirm on a \"$VERDICT\" record$asked: look at its journal for what ran (journalctl -b -1 inside it, at its next boot)")
    fi
    if [[ -n "$GUARD_LEFT" ]]; then
        warnings+=("the session guard is still in $DOMAIN's definition ($GUARD_LEFT): once it is shut off, run $SELF define")
    fi
    if ((${#UNMASKABLE[@]} > 0)); then
        warnings+=("the boot record names units the guard cannot mask: ${UNMASKABLE[*]} -- it says nothing runs btrbk at boot, but put them right before the next session")
    fi
    if ((SILENCES > 0)); then
        warnings+=("the session guard's reporter went silent $(count_of "$SILENCES" time) (the longest $(format_duration "$SILENCE_LONGEST")) with no reset of the VM: its reporter stopped, or the recovery OS hung -- the guard does not depend on it, and nothing was stopped for it")
    fi
    if [[ -n "$RESET_WATCH_DOWN" ]]; then
        warnings+=("the reset watch stopped ($RESET_WATCH_DOWN): from then, silence counted as a boot without the guard")
    fi
    for w in "${REPORT_LOSSES[@]}"; do
        warnings+=("the session guard's report: $w")
    done
    if ((G_GAP_SECS > 0)); then
        warnings+=("$(format_duration "$G_GAP_SECS") passed in $(count_of "$G_GAPS" gap) between two looks at the recovery OS, and the guard's deadlines and its silence did not count it: a host suspend (the recovery OS did not run either), or this script stopped (a Ctrl-Z) -- and then the recovery OS ran that long unwatched, every deadline moved out by as much; its report was still read in full after the gap")
    fi
    if [[ "$EGRESS_RESULT" == "NOT taken out"* ]]; then
        warnings+=("the egress rule was $EGRESS_RESULT")
    fi
    for w in "${warnings[@]}"; do
        lines+=$'\n'"  Warnings      $w"
        joined+="; $w"
    done
    if [[ "$UNATTENDED" == true && -n "$UNATTENDED_FAILED" ]]; then
        unattended_line=$'\n'"  Unattended    STOPPED at $UNATTENDED_FAILED -- the recovery OS was powered off; nothing after that step ran"
        if [[ -n "$UNATTENDED_HALF" ]]; then
            unattended_line+=$'\n'"  Unattended    WARNING: $UNATTENDED_HALF"
        fi
    elif [[ "$UNATTENDED" == true ]]; then
        unattended_line=$'\n'"  Unattended    every step done; rebooted into kernel ${KERNEL:-unknown}"
    fi
    cat <<EOF
Session done -- $LABEL
  Disk          $DISK ($DISK_DEV)
  VM ran        $(format_duration $((SECONDS - VM_START))); whole session $(format_duration $((SECONDS - SESSION_START)))$unattended_line
  Guard         $GUARD_RESULT
  Claim         $claim
  Given back    detached; $HOLDER_RESULT; btrfs device scan $SCAN_RESULT; $MOUNT_RESULT
  Egress rule   $EGRESS_RESULT
  Lock          released$lines
  Next          the next backup run reads the updated OS (RECOVERY OS in its report)
EOF
    logger -t "$LOG_TAG" -- "session for $LABEL done: guard $GUARD_RESULT; claim $claim; scan $SCAN_RESULT; $MOUNT_RESULT${UNATTENDED_FAILED:+; unattended update stopped at $UNATTENDED_FAILED}$joined" || :
    session_exit "$(session_status)"
}

# ---------------------------------------------------------------------------
# session-end
# ---------------------------------------------------------------------------
cmd_session_end() {
    local state xml attached=() src judged=0 rc
    require_root
    load_targets
    resolve_label "$1"
    resolve_os_state
    if read_record "$HOLDER_FILE"; then
        HOLDER_PID=$REC_PID
        DISK=$REC_DEV
    fi
    state="$(virsh_ domstate "$DOMAIN" 2>&1)" || refuse "cannot read the state of $DOMAIN: $state"
    if [[ "$state" != "shut off" ]]; then
        refuse "$DOMAIN is $state -- session-end never stops a running recovery OS. Power it off from inside it (virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN), then run this again"
    fi
    xml="$(virsh_ dumpxml --inactive "$DOMAIN" 2>&1)" || refuse "cannot read $DOMAIN's definition: $xml"
    mapfile -t attached < <(disk_sources "$xml")
    # A session that ended before giving it back left its guard behind.
    if guard_in "$xml"; then
        GUARDED=true
    fi
    # The egress rule is taken out once no recovery OS runs: this one is
    # shut off; another drive's may still need it.
    if [[ -e "$EGRESS_FILE" ]] && ! other_domain_running; then
        EGRESS_OWNED=true
    fi
    if [[ -z "$DISK" && ${#attached[@]} -eq 0 && "$GUARDED" != true && ! -e "$GUARD_STATE_FILE" ]]; then
        stop_console_bridge
        if [[ "$EGRESS_OWNED" == true ]]; then
            egress_remove
        fi
        log "nothing to end for $LABEL: no holder record, no disk attached to $DOMAIN and no session guard"
        [[ "$EGRESS_RESULT" != "NOT taken out"* ]] || return 5
        return 0
    fi
    # The domain's definition changes only under the maintenance lock. This
    # session's holder holds it while it lives; with none alive, take it -- or
    # another session could be defining its own guard into the domain at this
    # very moment. Nothing to detach or redefine: no lock is needed (another
    # job may hold it, and a stale holder record can still be cleared).
    if ((${#attached[@]} > 0)) || [[ "$GUARDED" == true ]]; then
        if [[ -z "$HOLDER_PID" ]] || ! holder_alive "$HOLDER_PID" "$DISK"; then
            take_lock
        fi
    fi
    # The guard's report is judged before anything is removed: it is the only
    # record of whether btrbk could run in the recovery OS.
    if [[ "$GUARDED" == true || -e "$GUARD_STATE_FILE" ]]; then
        judge_saved_guard "$GUARD_STATE_FILE" "$GUARD_FILE" "$state" "$RESET_FILE"
        judged=$GJ_STATUS
        if ((judged == 0)); then
            log "session guard: $GJ_TEXT"
        else
            warn "session guard: $GJ_TEXT"
        fi
    fi
    if [[ -z "$DISK" && ${#attached[@]} -eq 0 ]]; then
        if [[ "$GUARDED" != true ]]; then
            rm -f -- "$GUARD_XML_FILE" "$GUARD_FILE" "$GUARD_FILE".[0-9]* "$GUARD_STATE_FILE" "$RESET_FILE"
        elif remove_guard; then
            log "session-end: no disk to give back for $LABEL; the session guard was still in $DOMAIN's definition and is out of it now"
        elif ((judged == 0)); then
            judged=5
        fi
        stop_console_bridge
        if [[ "$EGRESS_OWNED" == true ]]; then
            egress_remove
        fi
        if [[ "$EGRESS_RESULT" == "NOT taken out"* ]] && ((judged == 0)); then
            judged=5
        fi
        release_lock
        exit "$judged"
    fi
    if [[ -z "$DISK" ]]; then
        resolve_disk
    fi
    for src in "${attached[@]}"; do
        if [[ "$src" != "$DISK" ]]; then
            refuse "$DOMAIN holds $src, which is not $LABEL's disk ($DISK) -- end that session with its own label"
        fi
    done
    if ((${#attached[@]} > 0)); then
        ATTACHED=true
    fi
    # Whatever ran may have written the filesystem: rescan it. And unless its
    # state says it was never resumed, it may have changed the OS: record it.
    STARTED=true
    RESUME_ATTEMPTED=true
    if read_guard_state "$GUARD_STATE_FILE" && [[ -z "$GS_RESUMED" ]]; then
        RESUME_ATTEMPTED=false
    fi
    DISK_DEV="$(readlink -f -- "$DISK" 2>/dev/null)" || DISK_DEV=$DISK
    if ! return_disk; then
        keep_after_failure
        exit 4
    fi
    log "session-end: $LABEL's disk is the host's again (btrfs device scan $SCAN_RESULT; $MOUNT_RESULT)"
    log "DAS maintenance lock: $(lock_report)"
    rc=$(session_status)
    if ((judged > rc)); then
        rc=$judged
    fi
    exit "$rc"
}

# ---------------------------------------------------------------------------
# define, status, screenshot
# ---------------------------------------------------------------------------
# The checks every domain definition must pass before define replaces it: it
# is shut off, has no disk, and -- $2 "legacy" -- nothing else of a session or
# of libvirt's own that a retirement would lose. $1 the domain. Prints
# nothing; refuses.
check_redefinable() {
    local state xml sources out snaps
    state="$(virsh_ domstate "$1" 2>&1)" || refuse "cannot read the state of $1: $state"
    if [[ "$state" != "shut off" ]]; then
        refuse "$1 is $state -- define replaces or retires only a shut-off domain"
    fi
    xml="$(virsh_ dumpxml --inactive "$1" 2>&1)" || refuse "cannot read $1's definition: $xml"
    sources="$(disk_sources "$xml")"
    if [[ -n "$sources" ]]; then
        refuse "$1 has a disk attached (${sources//$'\n'/, }) -- a session was not ended: $SELF session-end <label>"
    fi
    [[ "${2:-}" == legacy ]] || return 0
    if guard_in "$xml"; then
        refuse "$1 still carries a session guard in its definition -- a session was not ended; end it with the release that started it, then run define again"
    fi
    out="$(virsh_ dominfo "$1" 2>&1)" || refuse "cannot read $1's information: $out"
    if ! grep -Eq '^Managed save:[[:space:]]+no$' <<<"$out"; then
        refuse "$1 has a managed-save image, or whether it has one cannot be told -- retiring it would throw that state away; that is your decision (virsh --connect $LIBVIRT_URI managedsave-remove $1), then run define again"
    fi
    snaps="$(virsh_ snapshot-list --name "$1" 2>&1)" || refuse "cannot list $1's snapshots: $snaps"
    if [[ -n "${snaps//[[:space:]]/}" ]]; then
        refuse "$1 has libvirt snapshots (${snaps//$'\n'/ }) -- define never retires a domain that has them; they are yours to keep or remove"
    fi
}

# Define one domain per role = "mirror" target from the template, and retire
# the shared domain of before (recovery-os-updater) once nothing is lost by
# it: shut off, no disk, no guard, no managed-save image, no snapshots. Every
# check, of every domain, comes before the first change. Only these names are
# ever touched: no other VM, and none of their snapshots, is read or changed.
cmd_define() {
    local label loader template labels=() name out ids=() id legacy=false
    require_root
    [[ -r "$DOMAIN_TEMPLATE" ]] || refuse "the domain template $DOMAIN_TEMPLATE is missing -- install the project (cmake --install) first"
    name="$(xml_value "$NAME_RE" "$DOMAIN_TEMPLATE")" || refuse "$DOMAIN_TEMPLATE names no domain"
    if [[ "$name" != "$DOMAIN_BASE" ]]; then
        refuse "$DOMAIN_TEMPLATE defines '$name', not $DOMAIN_BASE"
    fi
    loader="$(xml_value "$LOADER_RE" "$DOMAIN_TEMPLATE")" || refuse "$DOMAIN_TEMPLATE names no firmware loader"
    # libvirt creates each VM's variable store from this template at its
    # first start, and keeps it afterwards; define never touches it.
    template="$(xml_value "$TEMPLATE_RE" "$DOMAIN_TEMPLATE")" || refuse "$DOMAIN_TEMPLATE names no NVRAM template"
    [[ -r "$TEST_ROOT$loader" ]] || refuse "the UEFI firmware $loader is not installed (Arch: edk2-ovmf) -- $DOMAIN_TEMPLATE names it"
    [[ -r "$TEST_ROOT$template" ]] || refuse "the UEFI variable template $template is not installed (Arch: edk2-ovmf)"
    load_targets
    for label in "${TARGET_LABELS[@]}"; do
        [[ "${T_ROLE[$label]}" != mirror ]] || labels+=("$label")
    done
    ((${#labels[@]} > 0)) || refuse "$DAS_CONFIG has no role = \"mirror\" target: there is no recovery drive to define a domain for"
    # Every check first.
    for label in "${labels[@]}"; do
        resolve_label "$label"
        domain_identity "$label"
        for id in "${ids[@]}"; do
            if [[ "$id" == "$D_UUID" || "$id" == "$D_MAC" ]]; then
                refuse "two recovery drives' domains would share an identity ($id) -- rename one target's label"
            fi
        done
        ids+=("$D_UUID" "$D_MAC")
        if virsh_ dominfo "$DOMAIN" >/dev/null 2>&1; then
            check_redefinable "$DOMAIN"
        fi
    done
    if virsh_ dominfo "$LEGACY_DOMAIN" >/dev/null 2>&1; then
        check_redefinable "$LEGACY_DOMAIN" legacy
        legacy=true
    fi
    for label in "${labels[@]}"; do
        resolve_label "$label"
        render_domain_xml || refuse "$RENDER_FAILURE"
        if virsh_ dominfo "$DOMAIN" >/dev/null 2>&1; then
            log "updating $DOMAIN (for $label) from $DOMAIN_TEMPLATE"
        else
            log "defining $DOMAIN (for $label) from $DOMAIN_TEMPLATE"
        fi
        out="$(virsh_ define --validate "$DOMAIN_XML" 2>&1)" || refuse "virsh define of $DOMAIN failed: $out"
        log "$out"
        if ! virsh_ dominfo "$DOMAIN" >/dev/null 2>&1; then
            refuse "virsh define reported success, but $DOMAIN is not defined"
        fi
        rm -f -- "$DOMAIN_XML"
    done
    if [[ "$legacy" == true ]]; then
        # Its NVRAM goes with it: firmware state of that one VM, nothing of
        # either drive's (each drive's own boot entries live on its ESP).
        out="$(virsh_ undefine --nvram "$LEGACY_DOMAIN" 2>&1)" || refuse "could not retire the shared domain $LEGACY_DOMAIN: $out -- the per-drive domains are defined; run define again"
        if virsh_ dominfo "$LEGACY_DOMAIN" >/dev/null 2>&1; then
            refuse "virsh undefine reported success, but $LEGACY_DOMAIN is still defined"
        fi
        log "retired the shared domain $LEGACY_DOMAIN (one domain per recovery drive now): $out"
    fi
}

# One drive's domain, as status shows it: its state, attached disk and
# session guard (judged as session-end would, without removing anything).
# Sets ST_RC to 6 (5 on a "no" record) when its guard is not engaged or
# cannot be judged.
status_of_domain() {
    local state xml sources names
    ST_RC=0
    state="$(current_state)"
    printf 'Domain            %s (%s): %s\n' "$DOMAIN" "$LABEL" "$state"
    if xml="$(virsh_ dumpxml "$DOMAIN" 2>&1)"; then
        sources="$(disk_sources "$xml")"
        printf '  Attached disk   %s\n' "${sources:-none}"
    else
        printf '  Attached disk   unknown (virsh: %s)\n' "$xml"
    fi
    if ! xml="$(virsh_ dumpxml --inactive "$DOMAIN" 2>&1)"; then
        # Not defined at all is no guard; anything else cannot be told.
        if names="$(virsh_ list --all --name 2>&1)" && ! grep -qxF -- "$DOMAIN" <<<"$names"; then
            printf '  Session guard   none (%s is not defined: %s define)\n' "$DOMAIN" "$SELF"
        else
            printf '  Session guard   unknown (virsh: %s)\n' "$xml"
            ST_RC=6
        fi
    elif guard_in "$xml"; then
        if [[ ! -e "$GUARD_STATE_FILE" ]]; then
            printf '  Session guard   in the definition; cannot be judged: no session state for it (a session not ended, or a host restart cleared /run): session-end %s\n' "$LABEL"
            ST_RC=6
        else
            judge_saved_guard "$GUARD_STATE_FILE" "$GUARD_FILE" "$state" "$RESET_FILE"
            printf '  Session guard   in the definition; %s\n' "$GJ_TEXT"
            ST_RC=$GJ_STATUS
        fi
    else
        printf '  Session guard   none\n'
    fi
}

cmd_status() {
    local f any=false rc=0 label
    require_root
    load_targets
    for label in "${TARGET_LABELS[@]}"; do
        [[ "${T_ROLE[$label]}" == mirror ]] || continue
        resolve_label "$label"
        status_of_domain
        if ((ST_RC > rc)); then
            rc=$ST_RC
        fi
    done
    if virsh_ dominfo "$LEGACY_DOMAIN" >/dev/null 2>&1; then
        printf 'Domain            %s: %s -- the shared domain of before; %s define retires it\n' "$LEGACY_DOMAIN" "$(DOMAIN=$LEGACY_DOMAIN current_state)" "$SELF"
    fi
    shopt -s nullglob
    for f in "$STATE_DIR"/*.holder; do
        any=true
        if ! read_record "$f"; then
            printf 'Holder            %s: unreadable record\n' "$(basename -- "$f" .holder)"
        elif holder_alive "$REC_PID" "$REC_DEV"; then
            printf 'Holder            %s: pid %s, alive, claims %s%s\n' "$(basename -- "$f" .holder)" "$REC_PID" "$REC_DEV" \
                "${REC_UNIT:+, scope $REC_UNIT: $(systemctl is-active "$REC_UNIT" 2>/dev/null || :)}"
        else
            printf 'Holder            %s: pid %s NOT RUNNING (stale record for %s)\n' "$(basename -- "$f" .holder)" "$REC_PID" "$REC_DEV"
        fi
    done
    shopt -u nullglob
    if [[ "$any" != true ]]; then
        printf 'Holder            none\n'
    fi
    printf 'Maintenance lock  %s\n' "$(lock_report)"
    printf 'Egress rule       %s\n' "$(egress_report)"
    # 6 (5 on a "no" record) when a guard's report says it is not engaged, or
    # cannot be judged.
    return "$rc"
}

cmd_screenshot() {
    local out=$2 state tmp err
    require_root
    load_targets
    resolve_label "$1"
    state="$(current_state)"
    if [[ "$state" != running ]]; then
        refuse "$DOMAIN is $state -- a screenshot needs it running"
    fi
    tmp="$(mktemp -d)"
    if ! err="$(virsh_ screenshot "$DOMAIN" "$tmp/screen" 2>&1)"; then
        rm -rf -- "$tmp"
        refuse "virsh screenshot failed: $err"
    fi
    if ! err="$(magick "$tmp/screen" "png:$out" 2>&1)"; then
        rm -rf -- "$tmp"
        refuse "magick could not convert the screenshot: $err"
    fi
    rm -rf -- "$tmp"
    log "screenshot of $DOMAIN written to $out"
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
check_knobs() {
    if [[ ! "$POLL_SECS" =~ ^[[:digit:]]+(\.[[:digit:]]+)?$ || ! "$POLL_SECS" =~ [123456789] || ! "$MINUTE_SECS" =~ ^[123456789][[:digit:]]*$ ||
        ! "$GRACE_SECS" =~ ^[123456789][[:digit:]]*$ || ! "$GUARD_SECS" =~ ^[123456789][[:digit:]]{0,5}$ || ! "$RESEND_SECS" =~ ^[123456789][[:digit:]]{0,4}$ ||
        ! "$CLOCK_GAP_SECS" =~ ^[123456789][[:digit:]]{0,5}$ || ! "$UPDATE_SECS" =~ ^[123456789][[:digit:]]{0,5}$ ||
        ! "$AGENT_SECS" =~ ^[123456789][[:digit:]]{0,5}$ || ! "$AGENT_GRACE_SECS" =~ ^[123456789][[:digit:]]{0,5}$ ]]; then
        printf 'recovery-os-vm.sh: DAS_RECOVERY_VM_POLL_SECS, _MINUTE_SECS, _GRACE_SECS, _GUARD_SECS, _RESEND_SECS, _CLOCK_GAP_SECS, _UPDATE_SECS, _AGENT_SECS and _AGENT_GRACE_SECS take numbers above 0\n' >&2
        exit 2
    fi
    # It goes into a shell script in the recovery OS: letters, digits and
    # spaces only.
    if [[ ! "$EGRESS_ORG" =~ ^[${ASCII_LETTERS}[:digit:]][${ASCII_LETTERS}[:digit:]\ ]*$ ]]; then
        printf 'recovery-os-vm.sh: DAS_RECOVERY_VM_EGRESS_ORG takes letters, digits and spaces (an AS number: AS209)\n' >&2
        exit 2
    fi
    if [[ -n "$TEST_ROOT" ]]; then
        warn "TEST ROOT (DAS_RECOVERY_VM_TEST_ROOT): host paths are under $TEST_ROOT -- this is NOT the lock backups take"
    fi
}

main() {
    (($# >= 1)) || usage
    local command=$1
    shift
    case "$command" in
        -h | --help | help)
            usage_text
            exit 0
            ;;
    esac
    check_knobs
    case "$command" in
        define)
            (($# == 0)) || usage
            cmd_define
            ;;
        session) cmd_session "$@" ;;
        session-end)
            if (($# != 1)) || [[ -z "$1" ]]; then usage; fi
            cmd_session_end "$1"
            ;;
        status)
            (($# == 0)) || usage
            cmd_status
            ;;
        screenshot)
            if (($# != 2)) || [[ -z "$1" || -z "$2" ]]; then usage; fi
            cmd_screenshot "$1" "$2"
            ;;
        console-socket)
            if (($# != 2)) || [[ -z "$1" || -z "$2" ]]; then usage; fi
            cmd_console_socket "$1" "$2"
            ;;
        history)
            (($# <= 1)) || usage
            cmd_history "${1:-}"
            ;;
        clean-runs)
            if (($# != 1)) || [[ -z "$1" ]]; then usage; fi
            cmd_clean_runs "$1"
            ;;
        *) usage ;;
    esac
}

# Sourced (the test suite's classifier checks): define, never run.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
