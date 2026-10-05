#!/bin/bash
# recovery-os-vm.sh - update a recovery drive's own OS by booting it in a VM
# Version: 1.0.0
# Date: 2026-10-04
#
# Each role = "mirror" target (a 2 TB recovery drive) carries a fully
# independent install: its own ESP on partition 1, its own root as subvolume
# @ on partition 2 -- and that partition-2 filesystem also receives backups
# from the host. This boots that OS in the libvirt domain recovery-os-updater
# with the WHOLE physical disk passed through, so it can be updated from
# inside -- keyring, full upgrade, reboot, verify -- without rebooting the
# workstation. bd DAS-Backup-Manager-7wb.
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
#          verdict: will, may or no. "will" is refused; "may" and "no" go on,
#          the guard enforcing. The record must also be at most
#          MAX_RECORD_AGE_DAYS old and made after this drive's last session
#          (the OS may have changed in it; the time of each session is kept
#          in recovery-os-vm-sessions beside the record, written before the
#          OS boots and again when the disk is back).
#          --accept-boot-record-risk lets a "will", an age or a session
#          through, loudly and into the summary -- never a record that is
#          missing, of another schema, or says nothing about this drive. The
#          rule that makes the verdict is btrdasd's; this only reads it (jq).
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
#          and every minute after -- which lands in a root-only file here.
#          Every line is judged: each boot must begin engaged, none may be
#          NOT engaged, and no silence longer than GUARD_SECS (a boot that
#          came without the guard says nothing). A failure on a "will" or
#          "may" record asks the recovery OS to shut down (exit 6); on "no"
#          it is a warning. The domain is started paused and resumed only if
#          its live definition carries the guard; otherwise it is destroyed
#          before it ran an instruction, and nothing boots. Lifted inside the
#          guest for the update (systemctl stop das-vm-guard: pacman cannot
#          replace a file something is mounted on), engaged again after.
#          Afterwards the domain is defined from the template again; the
#          template itself carries none of it. status and session-end judge
#          the report too, before session-end removes anything.
#   - This script never destroys a running recovery OS -- it could be in the
#     middle of an update. Every stop is an ACPI request (virsh shutdown),
#     sent again until it is off, and bounded (exit 3 at the bound). The one
#     exception, approved by the operator on 2026-10-05 (bd
#     DAS-Backup-Manager-0zm): `virsh destroy` of a domain started --paused
#     and never resumed, whose guest has not executed a single instruction --
#     no firmware, no OS, no write to the disk. destroy_never_resumed is the
#     only call, and it refuses once `virsh resume` has been tried. An
#     interrupt or an expired --timeout leaves the VM, the claim and the lock
#     as they are and says how to finish -- and what the guard was, or that
#     it was not judged.
#   - The host never writes the drive: no mount, no chroot, no copy. Every
#     write -- its ESP included -- is made by its own OS, booted in the VM.
#
# Usage (as root):
#   recovery-os-vm.sh define
#   recovery-os-vm.sh session <A|B|label> [--dry-run] [--timeout <minutes>]
#                             [--accept-boot-record-risk]
#   recovery-os-vm.sh session-end <A|B|label>
#   recovery-os-vm.sh status
#   recovery-os-vm.sh screenshot <file.png>
#
# A and B are shorthands for the one role = "mirror" target whose label
# contains that letter as a dash-separated word (system-recovery-A-2tb).
#
# Exit status:
#   0  done
#   1  refused, failed or interrupted before the VM ran -- nothing is held
#      (a started domain without the guard is destroyed while still paused)
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
#      the session guard did not confirm on a "no" record, or it could not
#      be taken out of the domain's definition again
#   6  the session guard did not confirm on a "will" or "may" record: the
#      recovery OS was asked to shut down (ACPI, again until it went; never
#      destroyed) and powered off, or powered off by itself without
#      confirming -- a session interrupted as it did, too -- and the disk was
#      given back. status and session-end exit 6 (5 on a "no" record) when
#      the guard's report says NOT engaged, is missing for a recovery OS that
#      ran, or cannot be read
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
#   DAS_RECOVERY_VM_POLL_SECS (5), DAS_RECOVERY_VM_MINUTE_SECS (60),
#   DAS_RECOVERY_VM_GRACE_SECS (600), DAS_RECOVERY_VM_GUARD_SECS (900),
#   DAS_RECOVERY_VM_RESEND_SECS (20)
#                               faster clocks
#   BTRDASD_BIN, DAS_CONFIG     as in backup-run.sh

set -euo pipefail
# One locale: bracket ranges then mean ASCII (bd 1bsx's class), and virsh
# says "shut off" -- what this script waits for -- in English.
export LC_ALL=C
# No job control: the holder must start in this process group, so that
# `setsid` gives it a session of its own without forking a second time.
set +m
# The holder's transient scope: <prefix>-<label>, -<n> for a replacement.
readonly HOLDER_UNIT_PREFIX="das-recovery-os-holder"

readonly DOMAIN="recovery-os-updater"
readonly LIBVIRT_URI="qemu:///system"
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

# Installed side by side by CMake: ${prefix}/lib/das-backup/{this script,libvirt/}.
readonly DOMAIN_XML="$SCRIPT_DIR/libvirt/$DOMAIN.xml"
# Must match indexer/src/scrub.rs MAINTENANCE_LOCK_PATH and backup-run.sh.
readonly MAINTENANCE_LOCK="$TEST_ROOT/run/das-maintenance.lock"
readonly STATE_DIR="$TEST_ROOT/run/das-recovery-os-vm"
readonly BY_ID="$TEST_ROOT/dev/disk/by-id"
readonly SYS_BLOCK="$TEST_ROOT/sys/block"

# The boot record (bd DAS-Backup-Manager-1yg): the schema this reads, and the
# age past which it no longer describes the drive (the nightly run rewrites it
# daily; a week of misses is not a record of today's OS).
readonly OS_STATE_SCHEMA=3
readonly MAX_RECORD_AGE_DAYS=8
# One line per fact; every string cleaned of control characters, since unit
# names and reasons come from the recovery OS itself. A runner is said by the
# name the guard would mask: the unit that runs btrbk, or for a cron file
# (a source with a slash) the cron daemon that runs it.
# shellcheck disable=SC2016 # jq's variables, not the shell's
readonly BOOT_RECORD_JQ='def clean: tostring | gsub("[[:cntrl:]]"; "?");
"schema\t\(.schema_version | clean)",
"schematype\t\(.schema_version | type)",
(if .schema_version == 3 then
   .drives[$label] as $d
   | if $d == null then "entry\tnone"
     else
       "entry\tpresent",
       "checked\t\($d.checked_epoch | clean)",
       "checkedtype\t\($d.checked_epoch | type)",
       (if $d.error != null then "error\t\(($d.error | clean) as $e | if $e == "" then "an error without text" else $e end)"
        elif $d.os == null then "error\tno OS was inspected"
        else empty end),
       (if $d.os == null then empty else
          "verdict\t\(($d.os.btrbk_at_boot.verdict // "") | clean)",
          (($d.os.btrbk_at_boot.reasons // [])[] | "reason\t\(clean)"),
          (($d.os.btrbk_at_boot.runners // [])[]
           | (if ((.source // "") | tostring | contains("/")) then .via else .source end) // ""
           | "runner\t\(clean)"),
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
# One line of the guard's report: what, then " boot " and the boot's id.
readonly REPORT_LINE_RE='^(.*) boot ([0123456789abcdef]{8}-[0123456789abcdef]{4}-[0123456789abcdef]{4}-[0123456789abcdef]{4}-[0123456789abcdef]{12})$'
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
readonly HELD_RE="^held (.+) pid ([0-9]+)$"

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
GUARD_FILE=""       # where that report lands: the port's file on this host
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
GUARD_LINES_SEEN=0  # report lines judged so far
GUARD_BOOTS_SEEN=0
GUARD_LIFTS_SEEN=0
GUARD_LAST_AT=0     # when the last of them arrived (SECONDS)
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
TARGET_ARG=""
SESSION_START=0
VM_START=0

# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------
log() {
    printf '%s\n' "$*"
    logger -t "$LOG_TAG" -- "$*" || :
}

warn() {
    printf 'WARNING: %s\n' "$*" >&2
    logger -p user.warning -t "$LOG_TAG" -- "WARNING: $*" || :
}

refuse() {
    printf 'REFUSED: %s\n' "$*" >&2
    logger -p user.err -t "$LOG_TAG" -- "REFUSED: $*" || :
    exit 1
}

usage_text() {
    cat <<EOF
Usage: $(basename -- "$SELF") COMMAND   (as root)

Boot one recovery drive's own OS in the libvirt domain $DOMAIN, with the
whole disk passed through, to update it without rebooting the workstation.

  define                 define or update the domain from
                         $DOMAIN_XML
                         (it must be shut off, with no disk attached)
  session <A|B|label> [--dry-run] [--timeout <minutes>]
          [--accept-boot-record-risk]
                         lend one role = "mirror" drive to the VM, boot it
                         with the session guard (btrbk cannot run in it),
                         wait until it powers off, give the disk back --
                         only when the nightly run's record does not say its
                         OS will run btrbk at boot, is fresh, and is newer
                         than the drive's last session (the flag lets a
                         "will", an age or a session through, loudly)
  session-end <A|B|label>
                         finish a session whose driver died (VM shut off);
                         judges the guard's report before removing anything
  status                 domain state, attached disk, the guard's report,
                         holder, lock
  screenshot <file.png>  the VM's screen as a PNG, while it runs

Exit status: 0 done; 1 refused, failed or interrupted, nothing held;
2 usage; 3 the recovery OS is still running and keeps the disk and the lock;
4 the disk could not be returned completely and is kept (finish 3 and 4 with
session-end); 5 done and given back, but see the summary's warnings (a dry
run that needed --accept-boot-record-risk exits 5 too); 6 the session guard
did not confirm on a "will" or "may" record, so the recovery OS was shut down
(never destroyed) and the disk given back. status and session-end exit 6 (5
on a "no" record) when the guard's report says it is not engaged, or cannot
be judged.
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
    if [[ ! "$1" =~ ^/[A-Za-z0-9/#+.:=@_-]+$ ]]; then
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
    elif [[ "$1" =~ [0-9]$ ]]; then
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
    [[ "$REC_PID" =~ ^[0-9]+$ && -n "$REC_DEV" ]]
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
    [[ "$1" =~ ^[0-9]+$ ]] || return 1
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
    elif [[ "$arg" =~ ^[A-Za-z]$ ]]; then
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
    if [[ ! "$SERIAL" =~ ^[A-Za-z0-9._-]+$ ]]; then
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
# off the disk cannot.
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
check_units() {
    local out states=() i busy=""
    # is-active exits non-zero whenever a unit is not active; the answer is
    # in its output, one line per unit, which is read strictly instead.
    out="$(systemctl is-active "${TARGET_UNITS[@]}" 2>&1)" || :
    mapfile -t states <<<"$out"
    if ((${#states[@]} != ${#TARGET_UNITS[@]})); then
        refuse "cannot tell whether ${TARGET_UNITS[*]} are running (systemctl said: $out)"
    fi
    for i in "${!TARGET_UNITS[@]}"; do
        case "${states[$i]}" in
            inactive | failed) ;;
            *) busy+="${busy:+, }${TARGET_UNITS[$i]} is ${states[$i]}" ;;
        esac
    done
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

check_domain_idle() {
    local state xml sources
    state="$(virsh_ domstate "$DOMAIN" 2>&1)" || refuse "cannot read the state of $DOMAIN: $state -- is it defined? $SELF define"
    if [[ "$state" != "shut off" ]]; then
        refuse "$DOMAIN is $state -- it must be shut off"
    fi
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
        if [[ ! "$line" =~ ^([^[:space:]]+)\ ([1-9][0-9]{0,17})$ ]]; then
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
            if [[ "$line" =~ ^([^[:space:]]+)\ ([1-9][0-9]{0,17})$ ]]; then
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
}

# Read this drive's boot record and go on only when it does not say btrbk
# will run when its OS boots -- before anything is taken. "may" goes on: the
# session guard is what keeps btrbk from running, and what the record names
# is masked by it. The record's own facts are shown whatever they say.
check_boot_record() {
    local file out rc=0 key value schemas=0 schema="" schematype="" entry="" checked="" checkedtype="" error=""
    local verdict="" units_state="" when now age p problems=() hints=() reasons=() units=() runners=() hint=""
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
            error) error=$value ;;
            verdict) verdict=$value ;;
            reason) reasons+=("$value") ;;
            runner) runners+=("$value") ;;
            units) units_state=$value ;;
            unit) units+=("$value") ;;
        esac
    done <<<"$out"
    if ((schemas != 1)); then
        refuse "the boot record $file is not one JSON document ($schemas found)"
    fi
    if [[ "$schematype" != number ]]; then
        refuse "the boot record $file is not one this script can read: its schema_version is not the number $OS_STATE_SCHEMA ('$schema', a $schematype)"
    fi
    if [[ "$schema" != "$OS_STATE_SCHEMA" ]]; then
        refuse "the boot record $file is schema $schema, not $OS_STATE_SCHEMA -- this script reads schema $OS_STATE_SCHEMA only (an older record has no btrbk-at-boot verdict); the next backup run writes it again"
    fi
    if [[ "$entry" != present ]]; then
        refuse "the boot record $file has no entry for '$LABEL' -- the nightly backup run writes one when it checks this drive; let one run with it attached"
    fi
    # A JSON number of whole seconds above 0: a string -- "015260430204" --
    # would reach bash arithmetic as octal, and 0 is no time at all.
    if [[ "$checkedtype" != number || ! "$checked" =~ ^[1-9][0-9]{0,17}$ ]]; then
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
    log "boot record for $LABEL ($file, schema $schema):"
    log "  checked        $when, $(age_text "$age")"
    if [[ "$units_state" == listed ]]; then
        log "  enabled units  $(IFS=,; p="${units[*]}"; printf '%s' "${p//,/, }") (${#units[@]})"
    else
        log "  enabled units  $units_state"
    fi
    log "  btrbk at boot  $verdict"
    for p in "${reasons[@]}"; do
        log "    - $p"
    done
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
    if [[ "$verdict" == will ]]; then
        problems+=("btrbk will run when this OS boots")
        hints+=("fix it from inside the recovery OS on bare metal, or check its config, then let the next backup run record it again (without starting btrbk: boot it with systemd.unit=emergency.target systemd.setenv=SYSTEMD_SULOGIN_FORCE=1 on its kernel line, remount / read-write if needed, mask the unit the record names, then systemctl poweroff -- never exit or Ctrl-D, and never Ctrl-Alt-Del: each boots it on; to stop at any prompt, hold the power button; see the disaster recovery guide)")
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
    [[ ${#1} -le 200 && "$1" =~ $UNIT_NAME_RE && "$1" != das-vm-guard* && ! "$1" =~ @\.[a-z]+$ ]]
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
    local u r w extra=() sorted=() over=() unit_b64 report_b64 dropin_b64 stub_b64 never_b64
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
    GUARD_EXPECT="das-vm-guard engaged ${#MASKS[@]} masks $(IFS=,; printf '%s' "${MASKS[*]}")"
    GUARD_EXPECT_LIFTED="das-vm-guard lifted ${#MASKS[@]} masks $(IFS=,; printf '%s' "${MASKS[*]}")"
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
# systemd's command-line parsing and sh.
guard_message_lines() {
    printf '%s\n' \
        "das-vm-guard: btrbk cannot run in this VM session (DAS recovery OS updater)." \
        "  To update, follow the first update in the disaster recovery guide. In short:" \
        "  1. lift it:    systemctl stop das-vm-guard   (if it failed: findmnt -rn -o TARGET | grep /btrbk | xargs -r -n1 umount)" \
        "  2. keyrings:   pacman -Sy archlinux-keyring cachyos-keyring" \
        "  3. drivers:    pin both worlds into the initramfs first (the guide, step 2)" \
        "  4. upgrade:    pacman -Su" \
        "  5. check:      pacman -Qkk btrbk   must find 0 altered files" \
        "  6. re-engage:  systemctl start das-vm-guard   (lifted, pacman hooks and units not masked can run btrbk)" \
        "  7. reboot, check uname -r, then systemctl poweroff"
}

# What runs in place of btrbk while the guard holds: it refuses, and says so
# on the kernel log -- the evidence that something tried. A credential of its
# own, so no line of it passes through systemd's parsing.
guard_stub_text() {
    cat <<'EOF'
#!/bin/sh
# das-vm-guard (DAS recovery OS updater): btrbk cannot run in this VM session.
echo "das-vm-guard: refused: btrbk $* (pid $$, parent $PPID $(cat /proc/$PPID/comm 2>/dev/null))" > /dev/kmsg 2>/dev/null
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
# the VM runs, one line through the virtio port with this boot's id -- the
# host notices silence, so a boot that comes without the guard is noticed too.
# The first line of a boot needs the guard active ("engaged"); later, an
# inactive one was stopped for the update ("lifted"), its masks still checked.
# A unit masked is one that cannot be started: masked outright, or this
# guard's empty unit (which systemd 262 loads as "bad-setting": the generator
# writes an empty credential as one newline) or that unit made whole by a
# drop-in of the OS's own ("loaded") -- in both cases only with the guard's
# never-true condition as the LAST drop-in. A state systemd cannot be asked
# about (a re-exec during an upgrade) or a unit between states sends nothing
# and looks again in 5 s. Waits for the port at most two minutes; ordered
# before nothing.
guard_report_text() {
    local script
    script="port=/dev/virtio-ports/$GUARD_PORT; boot=\$\$(cat /proc/sys/kernel/random/boot_id); n=0; "
    script+="while [ ! -e \"\$\$port\" ] && [ \"\$\$n\" -lt 120 ]; do sleep 1; n=\$\$((n + 1)); done; "
    script+="if [ ! -e \"\$\$port\" ]; then echo \"das-vm-guard: no port to the host\"; exit 0; fi; "
    script+="first=1; while :; do bad=; mode=; "
    script+="st=\$\$(systemctl show -P ActiveState $GUARD_UNIT) || { sleep 5; continue; }; "
    script+="case \"\$\$st\" in active) mode=engaged ;; inactive) if [ \"\$\$first\" = 1 ]; then bad=\"\$\$bad $GUARD_UNIT is inactive;\"; else mode=lifted; fi ;; activating|deactivating|reloading|refreshing) sleep 5; continue ;; *) bad=\"\$\$bad $GUARD_UNIT is \$\$st;\" ;; esac; "
    script+="ok=1; for u in ${MASKS[*]}; do s=\$\$(systemctl show -P LoadState \"\$\$u\") || { ok=0; break; }; "
    script+="case \"\$\$s\" in masked) ;; bad-setting|loaded) f=\$\$(systemctl show -P FragmentPath \"\$\$u\") || { ok=0; break; }; d=\$\$(systemctl show -P DropInPaths \"\$\$u\") || { ok=0; break; }; last=; for x in \$\$d; do last=\$\$x; done; "
    script+="if [ \"\$\$f\" != \"$GUARD_EARLY/\$\$u\" ] || [ \"\$\$last\" != \"$GUARD_EARLY/\$\$u.d/$GUARD_MASK_DROPIN.conf\" ]; then bad=\"\$\$bad \$\$u is not masked (\$\$s);\"; fi ;; "
    script+="*) bad=\"\$\$bad \$\$u is not masked (\$\$s);\" ;; esac; done; "
    script+="if [ \"\$\$ok\" = 0 ]; then sleep 5; continue; fi; "
    script+="if [ \"\$\$mode\" = engaged ]; then for p in ${GUARD_BTRBK_PATHS[*]}; do if [ -e \"\$\$p\" ] && { ! [ \"\$\$p\" -ef $GUARD_STUB ] || ! findmnt -rn --mountpoint \"\$\$(readlink -f \"\$\$p\")\" >/dev/null; }; then bad=\"\$\$bad \$\$p is not covered;\"; fi; done; fi; "
    script+="if [ -z \"\$\$bad\" ]; then line=\"das-vm-guard \$\$mode ${#MASKS[@]} masks $(IFS=,; printf '%s' "${MASKS[*]}") boot \$\$boot\"; else line=\"das-vm-guard NOT engaged:\$\$bad boot \$\$boot\"; fi; "
    script+="echo \"\$\$line\"; echo \"\$\$line\" >> \"\$\$port\"; first=0; sleep $GUARD_HEARTBEAT_SECS; done"
    cat <<EOF
[Unit]
Description=DAS VM session guard: report to the host
ConditionPathExists=!/etc/initrd-release
After=$GUARD_UNIT

[Service]
Type=simple
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

# Read a guard state file ($1) into GS_VERDICT, GS_ENGAGED, GS_LIFTED,
# GS_STARTED, GS_RESUMED. 1 when it is missing, unreadable, or incomplete.
read_guard_state() {
    local k v
    GS_VERDICT="" GS_ENGAGED="" GS_LIFTED="" GS_STARTED="" GS_RESUMED=""
    [[ -f "$1" && ! -L "$1" ]] || return 1
    while IFS='=' read -r k v; do
        case "$k" in
            verdict) GS_VERDICT=$v ;;
            engaged) GS_ENGAGED=$v ;;
            lifted) GS_LIFTED=$v ;;
            started) GS_STARTED=$v ;;
            resumed) GS_RESUMED=$v ;;
        esac
    done 2>/dev/null <"$1" || return 1
    [[ "$GS_VERDICT" =~ ^(will|may|no)$ && "$GS_ENGAGED" == "das-vm-guard engaged "* && "$GS_LIFTED" == "das-vm-guard lifted "* ]]
}

# Define the domain with the guard, and prove the definition carries it.
# Fail closed: without it, nothing boots.
define_guard() {
    local out xml missing
    check_path_chars "$GUARD_FILE"
    rm -f -- "$GUARD_FILE" "$GUARD_FILE".[0-9]* "$GUARD_STATE_FILE"
    if [[ -e "$GUARD_FILE" || -e "$GUARD_STATE_FILE" ]]; then
        refuse "cannot remove an old $GUARD_FILE or its state: a stale report could pass for this session's -- nothing was booted"
    fi
    if ! (umask 077 && guarded_domain_xml >"$GUARD_XML_FILE"); then
        rm -f -- "$GUARD_XML_FILE"
        refuse "cannot add the session guard to $DOMAIN_XML (each of '  </os>' and '  </devices>' once, and no SMBIOS strings or $GUARD_PORT port of its own, are needed) -- nothing was booted"
    fi
    write_guard_state "verdict=$VERDICT" "engaged=$GUARD_EXPECT" "lifted=$GUARD_EXPECT_LIFTED"
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
    if ! out="$(virsh_ define --validate "$DOMAIN_XML" 2>&1)"; then
        GUARD_LEFT="virsh define $DOMAIN_XML failed: $out"
    elif ! xml="$(virsh_ dumpxml --inactive "$DOMAIN" 2>&1)"; then
        GUARD_LEFT="cannot read $DOMAIN's definition back: $xml"
    elif guard_in "$xml"; then
        GUARD_LEFT="$DOMAIN's definition still carries it after defining $DOMAIN_XML"
    else
        GUARDED=false
        rm -f -- "$GUARD_XML_FILE" "$GUARD_FILE" "$GUARD_FILE".[0-9]* "$GUARD_STATE_FILE"
        log "took the session guard out of $DOMAIN's definition (defined from $DOMAIN_XML again)"
        return 0
    fi
    warn "the session guard is still in $DOMAIN's definition: $GUARD_LEFT. It only keeps btrbk from running in that VM; once it is shut off, run: $SELF define"
    return 1
}

# Judge every line of a guard report ($1) against the engaged ($2) and
# lifted ($3) lines. A line is "<what> boot <id>"; each boot's first line must
# be the engaged one; later lines of a boot, the engaged or the lifted one;
# a NOT engaged line, or any other, fails. Sets J_RESULT (none: no complete
# line; ok; failed, with J_WHY), J_LINES, J_BOOTS, J_LIFTS, and J_PARTIAL (an
# unfinished last line, cleaned for showing).
judge_report() {
    local file=$1 engaged=$2 lifted=$3 line="" body boot seen=" "
    J_RESULT=none J_WHY="" J_LINES=0 J_BOOTS=0 J_LIFTS=0 J_PARTIAL=""
    if [[ ! -e "$file" && ! -L "$file" ]]; then
        return 0
    fi
    if [[ -L "$file" || ! -f "$file" ]] || ! head -c 0 -- "$file" 2>/dev/null; then
        J_RESULT=failed J_WHY="the report $file cannot be read"
        return 0
    fi
    while IFS= read -r line; do
        J_LINES=$((J_LINES + 1))
        if [[ ! "$line" =~ $REPORT_LINE_RE ]]; then
            J_RESULT=failed J_WHY="a report line that is no report: '$(printable "$line")'"
            return 0
        fi
        body=${BASH_REMATCH[1]} boot=${BASH_REMATCH[2]}
        if [[ "$body" == "das-vm-guard NOT engaged:"* ]]; then
            J_RESULT=failed J_WHY="the recovery OS reports it NOT engaged:$(printable "${body#das-vm-guard NOT engaged:}")"
            return 0
        elif [[ "$seen" != *" $boot "* ]]; then
            seen+="$boot "
            J_BOOTS=$((J_BOOTS + 1))
            if [[ "$body" != "$engaged" ]]; then
                J_RESULT=failed J_WHY="boot $boot began without the guard engaged: '$(printable "$body")'"
                return 0
            fi
        elif [[ "$body" == "$lifted" ]]; then
            J_LIFTS=$((J_LIFTS + 1))
        elif [[ "$body" != "$engaged" ]]; then
            J_RESULT=failed J_WHY="a report line that is no report: '$(printable "$line")'"
            return 0
        fi
    done <"$file"
    J_PARTIAL="$(printable "$line")"
    if ((J_LINES > 0)); then
        J_RESULT=ok
    fi
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
        request_shutdown "the session guard did not confirm"
    fi
}

# Judge the recovery OS's report again, every line of it. $1 is "running" or
# "off". A failure is final; so is a report that shrank (rotated, or cut).
# Silence counts too: no line within GUARD_SECS of the start, or none for
# GUARD_SECS after the last one (a boot that came without the guard, or its
# reporter stopped) -- while it runs; and no line at all once it is off.
check_guard() {
    local partial=""
    if [[ "$GUARD_FAILED" == true ]]; then
        return 0
    fi
    judge_report "$GUARD_FILE" "$GUARD_EXPECT" "$GUARD_EXPECT_LIFTED"
    if [[ -n "$J_PARTIAL" ]]; then
        partial=" (an unfinished line: '$J_PARTIAL')"
    fi
    if [[ "$J_RESULT" == failed ]]; then
        guard_failed "$J_WHY" "$1"
        return 0
    fi
    if ((J_LINES < GUARD_LINES_SEEN)); then
        guard_failed "the report shrank from $GUARD_LINES_SEEN lines to $J_LINES (rotated, or cut): it cannot be judged" "$1"
        return 0
    fi
    if ((J_LINES > GUARD_LINES_SEEN)); then
        GUARD_LAST_AT=$SECONDS
        if [[ "$GUARD_CONFIRMED" != true ]]; then
            GUARD_CONFIRMED=true
            log "the session guard is engaged: $GUARD_EXPECT"
        fi
        if ((J_BOOTS > GUARD_BOOTS_SEEN && GUARD_BOOTS_SEEN > 0)); then
            log "the recovery OS booted again, and its guard is engaged ($(count_of "$J_BOOTS" boot) so far)"
        fi
        if ((J_LIFTS > GUARD_LIFTS_SEEN)); then
            log "the session guard is lifted for the update (its masks hold); btrbk can run there by hand until it is engaged again"
        fi
        GUARD_LINES_SEEN=$J_LINES GUARD_BOOTS_SEEN=$J_BOOTS GUARD_LIFTS_SEEN=$J_LIFTS
    fi
    if ((J_LINES > 0)); then
        GUARD_RESULT="engaged -- $GUARD_EXPECT ($(count_of "$J_BOOTS" boot); lifted $(count_of "$J_LIFTS" time))"
    fi
    if [[ "$1" == off ]]; then
        if ((J_LINES == 0)); then
            guard_failed "the recovery OS powered off without reporting$partial" off
        fi
    elif ((J_LINES == 0 && SECONDS - VM_START >= GUARD_SECS)); then
        guard_failed "no report from the recovery OS within $(format_duration "$GUARD_SECS") of its start$partial -- an OS whose systemd is older than 256 does not even see the guard" running
    elif ((J_LINES > 0 && SECONDS - GUARD_LAST_AT >= GUARD_SECS)); then
        guard_failed "the recovery OS stopped reporting: nothing for $(format_duration "$GUARD_SECS") after its last report -- a boot that came without the guard, or its reporter stopped" running
    fi
}

# The judgement of a session's guard for status and session-end, from its
# state ($1, the state file) and report ($2): GJ_TEXT, and GJ_STATUS -- 0, or
# 6 (5 on a "no" record) when it is not engaged or cannot be judged. $3 is
# the domain's state.
judge_saved_guard() {
    local partial=""
    GJ_STATUS=0
    if ! read_guard_state "$1"; then
        GJ_TEXT="cannot be judged: its session state ($1) is gone or unreadable -- a host restart clears /run"
        GJ_STATUS=6
        return 0
    fi
    judge_report "$2" "$GS_ENGAGED" "$GS_LIFTED"
    if [[ -n "$J_PARTIAL" ]]; then
        partial=" (an unfinished line: '$J_PARTIAL')"
    fi
    if [[ "$J_RESULT" == failed ]]; then
        GJ_TEXT="NOT engaged: $J_WHY"
    elif [[ "$J_RESULT" == ok ]]; then
        GJ_TEXT="$(judged_engaged)"
        return 0
    elif [[ -z "$GS_RESUMED" ]]; then
        if [[ -n "$GS_STARTED" ]]; then
            GJ_TEXT="never ran (started paused, never resumed): nothing to judge"
        else
            GJ_TEXT="never ran (never started): nothing to judge"
        fi
        return 0
    elif [[ "$3" != "shut off" && "$GS_RESUMED" =~ ^[0-9]+$ ]] && (($(date +%s) - GS_RESUMED < GUARD_SECS)); then
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
# --paused and never resumed has not executed a single guest instruction -- no
# firmware, no boot loader, no OS, no write to the disk. It is not a running
# recovery OS, and tearing it down interrupts nothing. Once `virsh resume` has
# been tried, never: a resumed recovery OS may be in the middle of anything,
# and only ACPI may ask it to stop.
destroy_never_resumed() {
    local out
    if [[ "$RESUME_ATTEMPTED" != false ]]; then
        warn "not destroying $DOMAIN: it was resumed, so it may have run"
        return 1
    fi
    if ! out="$(virsh_ destroy "$DOMAIN" 2>&1)"; then
        warn "virsh destroy (of the paused, never resumed domain): $out"
        return 1
    fi
    log "destroyed $DOMAIN while still paused, before it ran a single instruction"
}

# The domain was started paused: read its live definition and resume it only
# if it carries the guard. Without the guard there (another define or
# session-end ran between this session's check and its start), or with no
# definition to read, it is destroyed paused -- it never ran -- and nothing
# boots.
start_guarded() {
    local out xml missing
    write_guard_state "started=$(date +%s)"
    STARTED=true
    if ! out="$(virsh_ start --paused "$DOMAIN" 2>&1)"; then
        refuse "virsh start failed: $out"
    fi
    if ! xml="$(virsh_ dumpxml "$DOMAIN" 2>&1)"; then
        missing="cannot read the started domain's definition ($xml)"
    else
        missing="$(guard_missing "$xml")"
        [[ -z "$missing" ]] || missing="the started domain does not carry the session guard (missing: $missing)"
    fi
    if [[ -n "$missing" ]]; then
        if destroy_never_resumed; then
            refuse "$missing -- it was destroyed while still paused, before it ran a single instruction; nothing was booted"
        fi
        refuse "$missing -- and it could not be destroyed: it is still paused, and has run nothing (it is never resumed)"
    fi
    write_guard_state "resumed=$(date +%s)"
    RESUME_ATTEMPTED=true
    VM_START=$SECONDS
    if ! out="$(virsh_ resume "$DOMAIN" 2>&1)"; then
        refuse "virsh resume failed: $out"
    fi
}

# ---------------------------------------------------------------------------
# Taking and giving back
# ---------------------------------------------------------------------------
take_lock() {
    local holder
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
    if ! printf 'recovery-os VM session %s pid %s\n' "$LABEL" "$1" >"$MAINTENANCE_LOCK"; then
        warn "could not record pid $1 as the holder of $MAINTENANCE_LOCK"
    fi
}

# Empty the lock's holder record. Only while the lock is provably this
# session's -- this script holds it, or this session's holder still runs --
# so the record of whoever takes it next is never erased.
clear_lock_record() {
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
        log "released the DAS maintenance lock"
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
        if [[ -z "$SESSIONS_FILE" ]]; then
            warn "no place to record the end of this session (the boot record was never resolved)"
        elif ! record_session_time "$(date +%s)"; then
            warn "could not record the end of this session ($SESSIONS_FAILURE); its start is recorded, so the next session waits for a new boot record all the same"
        fi
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
    if [[ -z "$HOLDER_FAILURE" || -n "$LOCK_FD" ]]; then
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

keep_session() {
    local never_ran=false
    # Paused, and never resumed by this script: it has run nothing.
    if [[ "$RESUME_ATTEMPTED" == false && "$2" == paused ]]; then
        never_ran=true
    fi
    warn "leaving the session in place: $1"
    # What is left must be a live holder: it is what keeps both the claim and
    # the lock once this script has exited.
    if ! keep_claim; then
        warn "ONCE THIS SCRIPT EXITS NOTHING KEEPS THE HOST OFF $DISK: power the recovery OS off from inside it now, then run $SELF session-end $LABEL"
    fi
    # What the guard is, first: whether this OS may run btrbk decides what
    # the operator does next.
    if [[ "$never_ran" == true ]]; then
        warn "$DOMAIN was started paused and never resumed: it has not run a single instruction, so nothing -- guarded or not -- has booted"
    elif [[ "$GUARD_FAILED" == true ]]; then
        warn "THE RECOVERY OS IS RUNNING WITHOUT A CONFIRMED SESSION GUARD (${GUARD_RESULT#NOT confirmed: }): btrbk may run in it, against the backups on $DISK"
    elif [[ "$GUARD_CONFIRMED" == true ]]; then
        log "the session guard is engaged ($GUARD_EXPECT; its last report $(format_duration $((SECONDS - GUARD_LAST_AT))) ago, it reports every minute): btrbk cannot run in the recovery OS. From here nothing watches it: $SELF status judges its report again -- NOT engaged means a boot came without it"
    else
        warn "THE SESSION GUARD HAS NOT BEEN JUDGED: its report lands in $GUARD_FILE. Judge it now: $SELF status -- until it says engaged, treat the recovery OS as unguarded and power it off"
    fi
    cat >&2 <<EOF
The recovery OS keeps $DISK:
  - the disk holder (pid ${HOLDER_PID:-unknown}) still claims it, so nothing on
    this host can mount any partition of it;
  - it still holds $MAINTENANCE_LOCK, so backups and scrubs wait
    and the other jobs that mount targets defer.
Never 'systemctl stop' ${HOLDER_UNIT:-the scope of the holder}: that ends the claim and the lock
at once, and with this script gone nothing takes them again.
Nothing was detached and nothing was destroyed. To finish:
EOF
    if [[ "$never_ran" == true ]]; then
        cat >&2 <<EOF
  1. Tear it down -- it never ran, so nothing is cut short:
     virsh --connect $LIBVIRT_URI destroy $DOMAIN
  2. Then run:          $SELF session-end $LABEL
EOF
    elif [[ "$GUARD_CONFIRMED" == true && "$GUARD_FAILED" != true ]]; then
        cat >&2 <<EOF
  1. Open the console:  virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN
     Let the update finish, then power the recovery OS off from inside it.
  2. Then run:          $SELF session-end $LABEL
Only if it has hung, and accepting that an update in progress is cut short:
     virsh --connect $LIBVIRT_URI destroy $DOMAIN   -- then step 2.
EOF
    else
        cat >&2 <<EOF
  1. Power it off now: open the console (virt-viewer --connect $LIBVIRT_URI
     --attach $DOMAIN) and run systemctl poweroff there.
  2. If it has not gone within a minute, force it off -- a recovery OS whose
     guard is not confirmed must not run on: virsh --connect $LIBVIRT_URI destroy $DOMAIN
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
# The EXIT trap of a session
# ---------------------------------------------------------------------------
on_exit() {
    local rc=$? state
    set +e
    trap '' INT TERM HUP
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
        # it failed but did start): it ran nothing, so it may be torn down.
        # Paused after a resume was tried, it may have run: destroy_never_resumed
        # refuses it, and says so.
        if [[ "$state" == paused ]]; then
            destroy_never_resumed || :
            state="$(current_state)"
        fi
        if [[ "$state" != "shut off" ]]; then
            keep_session "${KEEP_REASON:-the driver stopped while the recovery OS is $state}" "$state"
            exit 3
        fi
        # It ran and is off: its report is judged before giving the disk back
        # takes the report away with the guard.
        if [[ "$RESUME_ATTEMPTED" == true ]]; then
            check_guard off
        fi
    fi
    if ! return_disk; then
        keep_after_failure
        exit 4
    fi
    if [[ "$GUARD_FAILED" == true && "$VERDICT" != no ]]; then
        warn "session for $LABEL ended early; the disk is the host's again and nothing is held -- but the session guard did not confirm on a \"$VERDICT\" record (${GUARD_RESULT#NOT confirmed: }): look at the recovery OS's journal for what ran (journalctl -b -1 inside it, at its next boot)"
        exit 6
    fi
    if [[ "$RESUME_ATTEMPTED" == true ]]; then
        log "session guard: $GUARD_RESULT"
    fi
    log "session for $LABEL ended early; the disk is the host's again and nothing is held"
    exit 1
}

# ---------------------------------------------------------------------------
# session
# ---------------------------------------------------------------------------
parse_session_args() {
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
            -*) usage ;;
            *)
                [[ -z "$TARGET_ARG" ]] || usage
                TARGET_ARG=$1
                ;;
        esac
        shift
    done
    [[ -n "$TARGET_ARG" ]] || usage
    if [[ -n "$TIMEOUT_MIN" && ! "$TIMEOUT_MIN" =~ ^[1-9][0-9]*$ ]]; then
        printf 'recovery-os-vm.sh: --timeout takes a whole number of minutes, at least 1\n' >&2
        exit 2
    fi
    if [[ "$DRY_RUN" == true && -n "$TIMEOUT_MIN" ]]; then
        printf 'recovery-os-vm.sh: --timeout has no meaning with --dry-run (nothing is booted)\n' >&2
        exit 2
    fi
}

# Ask the recovery OS to power off (ACPI) because of $1, again every
# RESEND_SECS -- one asked while it is in its firmware, boot menu or
# initramfs is dropped -- and wait until it has. Never destroyed: it may be in
# the middle of an update. Not off within GRACE_SECS: the session is kept
# (exit 3).
request_shutdown() {
    local out deadline state next=0 sent=0
    deadline=$((SECONDS + GRACE_SECS))
    while :; do
        if ((SECONDS >= next)); then
            sent=$((sent + 1))
            log "$1: asking the recovery OS to shut down$( ((sent > 1)) && printf ' (request %s)' "$sent")"
            if ! out="$(virsh_ shutdown "$DOMAIN" 2>&1)"; then
                warn "virsh shutdown: $out"
            fi
            next=$((SECONDS + RESEND_SECS))
        fi
        sleep "$POLL_SECS"
        state="$(current_state)"
        if [[ "$state" == "shut off" ]]; then
            log "the recovery OS shut down on request"
            return 0
        fi
        keep_claim || :
        ((SECONDS < deadline)) || break
    done
    KEEP_REASON="it did not power off within $(format_duration "$GRACE_SECS") of the shutdown request ($1) -- it may still be updating"
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
        check_guard running
        if [[ "$GUARD_SHUT_DOWN" == true ]]; then
            return 0
        fi
        if ((deadline > 0 && SECONDS >= deadline)); then
            request_shutdown "the --timeout of $TIMEOUT_MIN minute(s) has passed"
            return 0
        fi
    done
}

# 6 when the session guard did not confirm on a record that does not rule
# btrbk out; 5 when a session ended, everything given back, but something
# needs a look; else 0.
session_status() {
    if [[ "$GUARD_FAILED" == true && "$VERDICT" != no ]]; then
        echo 6
    elif [[ "$REENUMERATED" == true || "$MOUNT_RESULT" != "no partition mounted" || "$SCAN_RESULT" == FAILED* ]] ||
        [[ "$GUARD_FAILED" == true || -n "$GUARD_LEFT" ]] || ((${#UNMASKABLE[@]} > 0)) ||
        ((HOLDER_LOSSES > 0 || ${#BOOT_OVERRIDES[@]} > 0)); then
        echo 5
    else
        echo 0
    fi
}

cmd_session() {
    parse_session_args "$@"
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
    if [[ ! "$LABEL" =~ ^[A-Za-z0-9:_.-]+$ ]]; then
        refuse "the label '$LABEL' has characters a systemd unit name cannot carry"
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
    check_units
    check_not_mounted
    check_domain_idle
    check_no_live_holder
    install -d -m 0700 -- "$STATE_DIR"

    SESSION_START=$SECONDS
    trap on_exit EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    trap 'exit 129' HUP

    take_lock
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
        log "dry run: the session guard must report within $(format_duration "$GUARD_SECS") of the start, and again within that after each report"
        log "dry run done: lock taken and released, holder started and stopped, nothing defined or attached"
        if ((${#BOOT_OVERRIDES[@]} > 0)); then
            warn "dry run: the boot-record check was overridden (--accept-boot-record-risk) -- exit 5"
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
    # From here on the OS may change, so a record made before now no longer
    # describes it. Recorded first, or not booted at all.
    if ! record_session_time "$(date +%s)"; then
        refuse "cannot record the start of this session ($SESSIONS_FAILURE): without it, the next session could trust a boot record made before this one -- not booting"
    fi
    start_guarded
    log "the recovery OS is booting from $DISK"
    log "console: virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN (its VNC is a libvirt-private socket; '$SELF screenshot <file.png>' works too)"
    log "serial console: virsh --connect $LIBVIRT_URI console $DOMAIN"
    log "inside it, before updating: systemctl stop das-vm-guard; after: pacman -Qkk btrbk must find 0 altered files, then systemctl start das-vm-guard"

    wait_for_poweroff
    # It may have reported between two looks, or never.
    check_guard off

    log "the recovery OS powered off after $(format_duration $((SECONDS - VM_START))) -- giving $DISK back to the host"
    trap '' INT TERM HUP
    if ! return_disk; then
        keep_after_failure
        DONE=true
        exit 4
    fi
    DONE=true
    local claim="held throughout" warnings=() w lines="" joined="" asked=""
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
    for w in "${warnings[@]}"; do
        lines+=$'\n'"  Warnings      $w"
        joined+="; $w"
    done
    cat <<EOF
Session done -- $LABEL
  Disk          $DISK ($DISK_DEV)
  VM ran        $(format_duration $((SECONDS - VM_START))); whole session $(format_duration $((SECONDS - SESSION_START)))
  Guard         $GUARD_RESULT
  Claim         $claim
  Given back    detached; $HOLDER_RESULT; btrfs device scan $SCAN_RESULT; $MOUNT_RESULT
  Lock          released$lines
  Next          the next backup run reads the updated OS (RECOVERY OS in its report)
EOF
    logger -t "$LOG_TAG" -- "session for $LABEL done: guard $GUARD_RESULT; claim $claim; scan $SCAN_RESULT; $MOUNT_RESULT$joined" || :
    exit "$(session_status)"
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
    if [[ -z "$DISK" && ${#attached[@]} -eq 0 && "$GUARDED" != true && ! -e "$GUARD_STATE_FILE" ]]; then
        log "nothing to end for $LABEL: no holder record, no disk attached to $DOMAIN and no session guard"
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
        judge_saved_guard "$GUARD_STATE_FILE" "$GUARD_FILE" "$state"
        judged=$GJ_STATUS
        if ((judged == 0)); then
            log "session guard: $GJ_TEXT"
        else
            warn "session guard: $GJ_TEXT"
        fi
    fi
    if [[ -z "$DISK" && ${#attached[@]} -eq 0 ]]; then
        if [[ "$GUARDED" != true ]]; then
            rm -f -- "$GUARD_XML_FILE" "$GUARD_FILE" "$GUARD_FILE".[0-9]* "$GUARD_STATE_FILE"
        elif remove_guard; then
            log "session-end: no disk to give back for $LABEL; the session guard was still in $DOMAIN's definition and is out of it now"
        elif ((judged == 0)); then
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
    # Whatever ran may have written the filesystem: rescan it.
    STARTED=true
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
cmd_define() {
    local name loader template state xml sources out
    require_root
    [[ -r "$DOMAIN_XML" ]] || refuse "the domain definition $DOMAIN_XML is missing -- install the project (cmake --install) first"
    name="$(xml_value "$NAME_RE" "$DOMAIN_XML")" || refuse "$DOMAIN_XML names no domain"
    if [[ "$name" != "$DOMAIN" ]]; then
        refuse "$DOMAIN_XML defines '$name', not $DOMAIN"
    fi
    loader="$(xml_value "$LOADER_RE" "$DOMAIN_XML")" || refuse "$DOMAIN_XML names no firmware loader"
    # libvirt creates the VM's variable store from this template at its first
    # start, and keeps it afterwards; define never touches it.
    template="$(xml_value "$TEMPLATE_RE" "$DOMAIN_XML")" || refuse "$DOMAIN_XML names no NVRAM template"
    [[ -r "$TEST_ROOT$loader" ]] || refuse "the UEFI firmware $loader is not installed (Arch: edk2-ovmf) -- $DOMAIN_XML names it"
    [[ -r "$TEST_ROOT$template" ]] || refuse "the UEFI variable template $template is not installed (Arch: edk2-ovmf)"
    if virsh_ dominfo "$DOMAIN" >/dev/null 2>&1; then
        state="$(virsh_ domstate "$DOMAIN" 2>&1)" || refuse "cannot read the state of $DOMAIN: $state"
        if [[ "$state" != "shut off" ]]; then
            refuse "$DOMAIN is $state -- define replaces only a shut-off domain"
        fi
        xml="$(virsh_ dumpxml --inactive "$DOMAIN" 2>&1)" || refuse "cannot read $DOMAIN's definition: $xml"
        sources="$(disk_sources "$xml")"
        if [[ -n "$sources" ]]; then
            refuse "$DOMAIN has a disk attached (${sources//$'\n'/, }) -- a session was not ended: $SELF session-end <label>"
        fi
        log "updating $DOMAIN from $DOMAIN_XML"
    else
        log "defining $DOMAIN from $DOMAIN_XML"
    fi
    out="$(virsh_ define --validate "$DOMAIN_XML" 2>&1)" || refuse "virsh define failed: $out"
    log "$out"
    if ! virsh_ dominfo "$DOMAIN" >/dev/null 2>&1; then
        refuse "virsh define reported success, but $DOMAIN is not defined"
    fi
}

cmd_status() {
    local state xml sources f any=false rc=0 saved=()
    require_root
    state="$(current_state)"
    printf 'Domain            %s: %s\n' "$DOMAIN" "$state"
    if xml="$(virsh_ dumpxml "$DOMAIN" 2>&1)"; then
        sources="$(disk_sources "$xml")"
        printf 'Attached disk     %s\n' "${sources:-none}"
    else
        printf 'Attached disk     unknown (virsh: %s)\n' "$xml"
    fi
    if ! xml="$(virsh_ dumpxml --inactive "$DOMAIN" 2>&1)"; then
        printf 'Session guard     unknown (virsh: %s)\n' "$xml"
        rc=6
    elif guard_in "$xml"; then
        # Judged as session-end would, without removing anything.
        shopt -s nullglob
        saved=("$STATE_DIR"/*.guard.state)
        shopt -u nullglob
        if ((${#saved[@]} == 0)); then
            printf 'Session guard     in the definition; cannot be judged: no session state for it (a session not ended, or a host restart cleared /run): session-end <label>\n'
            rc=6
        fi
        for f in "${saved[@]}"; do
            judge_saved_guard "$f" "${f%.state}" "$state"
            printf 'Session guard     in the definition; %s\n' "$GJ_TEXT"
            if ((GJ_STATUS > rc)); then
                rc=$GJ_STATUS
            fi
        done
    else
        printf 'Session guard     none\n'
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
    # 6 (5 on a "no" record) when a guard's report says it is not engaged, or
    # cannot be judged.
    return "$rc"
}

cmd_screenshot() {
    local out=$1 state tmp err
    require_root
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
    if [[ ! "$POLL_SECS" =~ ^[0-9]+(\.[0-9]+)?$ || ! "$POLL_SECS" =~ [1-9] || ! "$MINUTE_SECS" =~ ^[1-9][0-9]*$ ||
        ! "$GRACE_SECS" =~ ^[1-9][0-9]*$ || ! "$GUARD_SECS" =~ ^[1-9][0-9]{0,5}$ || ! "$RESEND_SECS" =~ ^[1-9][0-9]{0,4}$ ]]; then
        printf 'recovery-os-vm.sh: DAS_RECOVERY_VM_POLL_SECS, _MINUTE_SECS, _GRACE_SECS, _GUARD_SECS and _RESEND_SECS take numbers above 0\n' >&2
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
            if (($# != 1)) || [[ -z "$1" ]]; then usage; fi
            cmd_screenshot "$1"
            ;;
        *) usage ;;
    esac
}

main "$@"
