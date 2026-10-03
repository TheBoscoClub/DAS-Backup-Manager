#!/bin/bash
# recovery-os-vm.sh - update a recovery drive's own OS by booting it in a VM
# Version: 1.0.0
# Date: 2026-10-03
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
#   - This script never destroys a running recovery OS -- it could be in the
#     middle of an update. An interrupt or an expired --timeout leaves the
#     VM, the claim and the lock as they are and says how to finish.
#   - The host never writes the drive: no mount, no chroot, no copy. Every
#     write -- its ESP included -- is made by its own OS, booted in the VM.
#
# Usage (as root):
#   recovery-os-vm.sh define
#   recovery-os-vm.sh session <A|B|label> [--dry-run] [--timeout <minutes>]
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
#   2  usage
#   3  the recovery OS is still running (an interrupt, or --timeout without
#      a shutdown): the claim and the lock are KEPT; finish with session-end
#   4  the disk could not be returned completely (a detach, or the holder's
#      exit, failed): the claim and the lock are KEPT; finish with session-end
#   5  the session ended and the disk was given back, but something needs a
#      look (see the summary): the claim was lost while the VM ran (taken
#      again or not), the drive re-enumerated during the session, a
#      partition was mounted afterwards, or the device scan failed
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
#   DAS_RECOVERY_VM_POLL_SECS (5), DAS_RECOVERY_VM_MINUTE_SECS (60),
#   DAS_RECOVERY_VM_GRACE_SECS (600)   faster clocks
#   BTRDASD_BIN, DAS_CONFIG     as in backup-run.sh

set -euo pipefail
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

# Installed side by side by CMake: ${prefix}/lib/das-backup/{this script,libvirt/}.
readonly DOMAIN_XML="$SCRIPT_DIR/libvirt/$DOMAIN.xml"
# Must match indexer/src/scrub.rs MAINTENANCE_LOCK_PATH and backup-run.sh.
readonly MAINTENANCE_LOCK="$TEST_ROOT/run/das-maintenance.lock"
readonly STATE_DIR="$TEST_ROOT/run/das-recovery-os-vm"
readonly BY_ID="$TEST_ROOT/dev/disk/by-id"
readonly SYS_BLOCK="$TEST_ROOT/sys/block"

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
                         lend one role = "mirror" drive to the VM, boot it,
                         wait until it powers off, give the disk back
  session-end <A|B|label>
                         finish a session whose driver died (VM shut off)
  status                 domain state, attached disk, holder, lock
  screenshot <file.png>  the VM's screen as a PNG, while it runs

Exit status: 0 done; 1 refused, failed or interrupted, nothing held;
2 usage; 3 the recovery OS is still running and keeps the disk and the lock;
4 the disk could not be returned completely and is kept (finish 3 and 4 with
session-end); 5 done and given back, but see the summary's warnings.
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
    warn "leaving the session in place: $1"
    # What is left must be a live holder: it is what keeps both the claim and
    # the lock once this script has exited.
    if ! keep_claim; then
        warn "ONCE THIS SCRIPT EXITS NOTHING KEEPS THE HOST OFF $DISK: power the recovery OS off from inside it now, then run $SELF session-end $LABEL"
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
  1. Open the console:  virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN
     Let the update finish, then power the recovery OS off from inside it.
  2. Then run:          $SELF session-end $LABEL
Only if it has hung, and accepting that an update in progress is cut short:
     virsh --connect $LIBVIRT_URI destroy $DOMAIN   -- then step 2.
EOF
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
        if [[ "$state" != "shut off" ]]; then
            keep_session "${KEEP_REASON:-the driver stopped while the recovery OS is $state}"
            exit 3
        fi
    fi
    if ! return_disk; then
        keep_after_failure
        exit 4
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

shutdown_on_timeout() {
    local out deadline state
    log "the --timeout of $TIMEOUT_MIN minute(s) has passed: asking the recovery OS to shut down"
    if ! out="$(virsh_ shutdown "$DOMAIN" 2>&1)"; then
        warn "virsh shutdown: $out"
    fi
    deadline=$((SECONDS + GRACE_SECS))
    while ((SECONDS < deadline)); do
        sleep "$POLL_SECS"
        state="$(current_state)"
        if [[ "$state" == "shut off" ]]; then
            log "the recovery OS shut down on request"
            return 0
        fi
        keep_claim || :
    done
    KEEP_REASON="it did not power off within $(format_duration "$GRACE_SECS") of the shutdown request -- it may still be updating"
    exit 3
}

wait_for_poweroff() {
    local deadline=0 last="running" state
    if [[ -n "$TIMEOUT_MIN" ]]; then
        deadline=$((SECONDS + TIMEOUT_MIN * MINUTE_SECS))
    fi
    log "waiting for the recovery OS to power off (state polled every ${POLL_SECS}s${TIMEOUT_MIN:+; shutdown requested after $TIMEOUT_MIN minute(s)})"
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
        if ((deadline > 0 && SECONDS >= deadline)); then
            shutdown_on_timeout
            return 0
        fi
    done
}

# 5 when a session ended, everything given back, but something needs a look;
# else 0.
session_status() {
    if [[ "$REENUMERATED" == true || "$MOUNT_RESULT" != "no partition mounted" || "$SCAN_RESULT" == FAILED* ]] ||
        ((HOLDER_LOSSES > 0)); then
        echo 5
    else
        echo 0
    fi
}

cmd_session() {
    local out
    parse_session_args "$@"
    require_root
    # No fallback: a holder in the login session ends with it, taking the
    # claim and the lock while the VM may still use the disk.
    if ! command -v systemd-run >/dev/null; then
        refuse "systemd-run is not available: the disk holder must run in a scope of its own, outside your login session, and there is no other way to put it there"
    fi
    load_targets
    resolve_label "$TARGET_ARG"
    if [[ ! "$LABEL" =~ ^[A-Za-z0-9:_.-]+$ ]]; then
        refuse "the label '$LABEL' has characters a systemd unit name cannot carry"
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
        log "dry run done: lock taken and released, holder started and stopped, nothing attached"
        exit 0
    fi

    attach_disk
    # A USB disk that re-enumerated since the claim is another device now.
    if [[ "$(readlink -f -- "$DISK")" != "$DISK_DEV" ]]; then
        refuse "$DISK no longer leads to $DISK_DEV (the drive re-enumerated) -- start again"
    fi
    STARTED=true
    VM_START=$SECONDS
    if ! out="$(virsh_ start "$DOMAIN" 2>&1)"; then
        refuse "virsh start failed: $out"
    fi
    log "the recovery OS is booting from $DISK"
    log "console: virt-viewer --connect $LIBVIRT_URI --attach $DOMAIN (its VNC is a libvirt-private socket; '$SELF screenshot <file.png>' works too)"
    log "serial console: virsh --connect $LIBVIRT_URI console $DOMAIN"

    wait_for_poweroff

    log "the recovery OS powered off after $(format_duration $((SECONDS - VM_START))) -- giving $DISK back to the host"
    trap '' INT TERM HUP
    if ! return_disk; then
        keep_after_failure
        DONE=true
        exit 4
    fi
    DONE=true
    local claim="held throughout" warnings=() w lines="" joined=""
    if ((HOLDER_LOSSES > 0)) && [[ "$CLAIM_GAP" == true ]]; then
        claim="LOST $HOLDER_LOSSES time(s); NOT claimed again -- see the warnings above"
        warnings+=("the claim was lost and not taken again: until the recovery OS was off, nothing kept this host off the drive")
    elif ((HOLDER_LOSSES > 0)); then
        claim="LOST $HOLDER_LOSSES time(s) and claimed again -- see the warnings above"
    fi
    if [[ "$REENUMERATED" == true ]]; then
        warnings+=("the disk re-enumerated during the session; the claim was on the old device")
    fi
    for w in "${warnings[@]}"; do
        lines+=$'\n'"  Warnings      $w"
        joined+="; $w"
    done
    cat <<EOF
Session done -- $LABEL
  Disk          $DISK ($DISK_DEV)
  VM ran        $(format_duration $((SECONDS - VM_START))); whole session $(format_duration $((SECONDS - SESSION_START)))
  Claim         $claim
  Given back    detached; $HOLDER_RESULT; btrfs device scan $SCAN_RESULT; $MOUNT_RESULT
  Lock          released$lines
  Next          the next backup run reads the updated OS (RECOVERY OS in its report)
EOF
    logger -t "$LOG_TAG" -- "session for $LABEL done: claim $claim; scan $SCAN_RESULT; $MOUNT_RESULT$joined" || :
    exit "$(session_status)"
}

# ---------------------------------------------------------------------------
# session-end
# ---------------------------------------------------------------------------
cmd_session_end() {
    local state xml attached=() src
    require_root
    load_targets
    resolve_label "$1"
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
    if [[ -z "$DISK" ]]; then
        if ((${#attached[@]} == 0)); then
            log "nothing to end for $LABEL: no holder record and no disk attached to $DOMAIN"
            return 0
        fi
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
    exit "$(session_status)"
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
    local state xml sources f any=false
    require_root
    state="$(current_state)"
    printf 'Domain            %s: %s\n' "$DOMAIN" "$state"
    if xml="$(virsh_ dumpxml "$DOMAIN" 2>&1)"; then
        sources="$(disk_sources "$xml")"
        printf 'Attached disk     %s\n' "${sources:-none}"
    else
        printf 'Attached disk     unknown (virsh: %s)\n' "$xml"
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
    if [[ ! "$POLL_SECS" =~ ^[0-9]+(\.[0-9]+)?$ || ! "$POLL_SECS" =~ [1-9] || ! "$MINUTE_SECS" =~ ^[1-9][0-9]*$ || ! "$GRACE_SECS" =~ ^[1-9][0-9]*$ ]]; then
        printf 'recovery-os-vm.sh: DAS_RECOVERY_VM_POLL_SECS, _MINUTE_SECS and _GRACE_SECS take numbers above 0\n' >&2
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
