#!/bin/bash
# shellcheck disable=SC2016,SC2034,SC2329
# SC2034: the target tables are read by mounted_is_target, extracted from
#   backup-run.sh, so the use site cannot be seen here.
# SC2329: the smartctl/findmnt/parent_disk stubs are called by that extracted code.
#
# tests/test_serial_identity.sh
#
# How backup-run.sh and backup-verify.sh read a drive's serial from
# `smartctl -i` (bd DAS-Backup-Manager-2kb1.17). The rule is the partition
# script's parse_drive_serial (bd DAS-Backup-Manager-tjph): exactly one line
# "Serial Number:" (ATA, NVMe) or "Serial number:" (SCSI, SAS), the WHOLE
# trimmed value, and smartctl's failure bits (exit & 3) refused. Before it both
# scripts took awk's third field of "Serial Number:" only, so a SAS drive was
# never identified, a value with a blank matched on its first word, and a
# smartctl that could not open the drive was believed.
#
# backup-run.sh's mounted_is_target decides whether btrbk may write to a path
# when a target has no mount_uuid: an unreadable or ambiguous serial must
# REFUSE. Both directions are pinned here: a right serial passes (either
# spelling, a SMART finding in the exit status does not matter), every other
# case refuses with a reason.
#
# The functions are extracted by sed; smartctl, findmnt and parent_disk are
# shell functions over fixture files; /dev is a fixture tree. No device, mount,
# lock or mailer is touched. RUN_SCRIPT / VERIFY_SCRIPT point the test at
# another copy of a script (the falsification: the pre-fix files).
set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN="${RUN_SCRIPT:-$ROOT/scripts/backup-run.sh}"
VERIFY="${VERIFY_SCRIPT:-$ROOT/scripts/backup-verify.sh}"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK:?}"' EXIT
mkdir -p "$WORK/dev" "$WORK/smart"

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

extract() { sed -n "/^$2() {/,/^}/p" "$1"; }
load() { # <script> <function...>
    local script="$1" fn body
    shift
    for fn in "$@"; do
        body="$(extract "$script" "$fn")"
        [[ -n "$body" ]] || { echo "FAIL: $fn not found in $script"; exit 1; }
        if [[ $fn == find_device_by_serial ]]; then
            # The fixture /dev, and plain existence for a fixture file.
            [[ "$body" == *'/dev/sd[a-z] /dev/sd[a-z][a-z]'* && "$body" == *'[[ -b "$dev" ]]'* ]] \
                || { echo "FAIL: find_device_by_serial no longer walks /dev/sd* with -b"; exit 1; }
            body="${body//\/dev\/sd\[a-z\] \/dev\/sd\[a-z\]\[a-z\]/$WORK/dev/sd[a-z]}"
            body="${body//'[[ -b "$dev" ]]'/'[[ -e "$dev" ]]'}"
        fi
        eval "$body"
    done
}
# backup-run.sh's pieces. parse_drive_serial is optional here: the pre-fix
# script has none, and the cases must then fail on behaviour, not on the load.
load "$RUN" mounted_is_target find_device_by_serial get_smart_summary
if [[ -n "$(extract "$RUN" parse_drive_serial)" ]]; then load "$RUN" parse_drive_serial read_drive_serial; fi
# backup-verify.sh defines read_drive_serial; keep it apart under another name.
vbody="$(extract "$VERIFY" read_drive_serial)"
[[ -n "$vbody" ]] || { echo "FAIL: read_drive_serial not found in $VERIFY"; exit 1; }
vbody="${vbody/read_drive_serial() \{/verify_read_drive_serial() \{}"
if [[ -n "$(extract "$VERIFY" parse_drive_serial)" ]]; then
    eval "$(extract "$VERIFY" parse_drive_serial)"
fi
eval "$vbody"

# --- fixtures ----------------------------------------------------------------
# smart <dev> <exit status> <output...>: what `smartctl -i <dev>` prints and exits with.
smart() {
    local dev="$1" rc="$2"
    shift 2
    printf '%s\n' "$@" >"$WORK/smart/$dev.out"
    printf '%s\n' "$rc" >"$WORK/smart/$dev.rc"
    : >"$WORK/dev/$dev"
}
smartctl() { # smartctl -i <dev>; reads the fixture, never calls itself
    local n="${2##*/}"
    [[ -f "$WORK/smart/$n.out" ]] || return 2
    cat "$WORK/smart/$n.out"
    return "$(cat "$WORK/smart/$n.rc")"
}
findmnt() { echo "$WORK/dev/sdm1"; }
parent_disk() { echo "$WORK/dev/sdm"; }

declare -A TARGET_MOUNTS=([t]=/mnt/t) TARGET_MOUNT_UUIDS=([t]="") DAS_SERIALS_LIST=([t]="SER1 SER2")
# identity <exit> <output...>: mounted_is_target's verdict for the disk under the mount.
identity() {
    local rc="$1"
    shift
    smart sdm "$rc" "$@"
    if mounted_is_target t; then echo "pass"; else echo "refuse"; fi
}
# The refusal text of an identity call (a subshell would lose the variable).
why() {
    local rc="$1"
    shift
    smart sdm "$rc" "$@"
    mounted_is_target t >/dev/null || true
    echo "$IDENTITY_WHY"
}

ATA='Serial Number:    SER1'
SAS='Serial number:  SER1'

echo "== backup-run.sh mounted_is_target (serial fallback: the btrbk write gate)"
check "ATA 'Serial Number:' of a listed serial passes" "$(identity 0 'Model Family: X' "$ATA")" pass
check "...the second listed serial passes too" "$(identity 0 'Serial Number: SER2')" pass
check "SAS 'Serial number:' of a listed serial passes" "$(identity 0 'Vendor: SEAGATE' "$SAS")" pass
check "a SMART finding in the exit status (bit 2+) still identifies" "$(identity 4 "$ATA")" pass
check "a serial that is not listed refuses" "$(identity 0 'Serial Number: OTHER')" refuse
check "...and the reason names the serial read" "$(why 0 'Serial Number: OTHER' | grep -c "OTHER")" 1
check "a value that only STARTS with a listed serial refuses (whole value)" "$(identity 0 'Serial Number: SER1 EXTRA')" refuse
check "two serial lines, the first listed, refuses" "$(identity 0 "$ATA" 'Serial number: SER9')" refuse
check "two identical serial lines refuses" "$(identity 0 "$ATA" "$SAS")" refuse
check "an empty serial value refuses" "$(identity 0 'Serial Number:')" refuse
check "no serial line at all refuses" "$(identity 0 'Model Family: X')" refuse
check "smartctl could not open the device (exit 2) refuses, even with a listed serial printed" "$(identity 2 "$ATA")" refuse
check "smartctl command-line failure (exit 1) refuses" "$(identity 1 "$ATA")" refuse
check "smartctl not found (exit 127) refuses" "$(identity 127 "$ATA")" refuse
check "a refusal always says why" "$([[ -n "$(why 2 "$ATA")" ]] && echo yes)" yes
check "a pass leaves no refusal text" "$(why 0 "$SAS")" ""

echo "== backup-run.sh find_device_by_serial"
rm -f "${WORK:?}/dev/sdm"
smart sda 0 'Serial Number: OTHER'
smart sdb 0 'Serial number: SER1'
check "finds a SAS drive by its serial" "$(find_device_by_serial SER1 || echo none)" "$WORK/dev/sdb"
smart sdc 0 'Serial Number: SER1'
check "a serial on two nodes is ambiguous: no device" "$(find_device_by_serial SER1 || echo none)" none
smart sdc 2 'Serial Number: SER1'
check "a node smartctl could not open is not a match, the readable one is" "$(find_device_by_serial SER1 || echo none)" "$WORK/dev/sdb"
rm -f "${WORK:?}/dev/sdb"
check "only an unreadable node carries it: no device" "$(find_device_by_serial SER1 || echo none)" none

echo "== backup-run.sh get_smart_summary (report row)"
smart sdm 0 "$SAS"
check "a SAS serial reaches the SMART row" "$(get_smart_summary "$WORK/dev/sdm" | cut -d'|' -f4)" SER1

echo "== backup-verify.sh read_drive_serial"
smart sdv 0 "$SAS"
check "SAS 'Serial number:' is read" "$(verify_read_drive_serial "$WORK/dev/sdv")" SER1
smart sdv 0 "$ATA"
check "ATA 'Serial Number:' is read" "$(verify_read_drive_serial "$WORK/dev/sdv")" SER1
smart sdv 0 'Serial Number: SER1 EXTRA'
check "the whole value, not its first word" "$(verify_read_drive_serial "$WORK/dev/sdv")" "SER1 EXTRA"
smart sdv 0 "$ATA" "$SAS"
check "two serial lines: empty" "$(verify_read_drive_serial "$WORK/dev/sdv")" ""
smart sdv 2 "$ATA"
check "smartctl could not open the drive: empty" "$(verify_read_drive_serial "$WORK/dev/sdv")" ""
smart sdv 4 "$ATA"
check "a SMART finding in the exit status still reads it" "$(verify_read_drive_serial "$WORK/dev/sdv")" SER1

if [[ $fails -eq 0 ]]; then
    echo "SERIAL IDENTITY SUITE GREEN"
else
    echo "$fails FAILED"
    exit 1
fi
