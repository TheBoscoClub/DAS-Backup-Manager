#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: DAS_MODEL_PATTERN, DAS_DEVICES and DRIVE_MAP are read by the function
#   extracted from backup-verify.sh, so the use site cannot be seen here.
# SC2329: the log_*/read_drive_serial stubs are called by that same code.
#
# tests/test_verify_enclosure.sh
#
# backup-verify.sh's detect_das_drives() picks the DAS disks by
# [das].model_pattern (bd DAS-Backup-Manager-zk89). Since bd yi8m the pattern
# is a SUBSTRING of the disk's model (blanks removed), as backup-run.sh matches
# the USB product string, and an empty pattern is no filter. It was an exact
# match, which a configured product string never was; an enclosure's model is
# not the string an operator copies from `lsusb`.
#
# Substring is the right reading: it agrees with backup-run.sh, which reads the
# same key. The widening cannot pick another device CLASS, because the USB
# transport test runs first and a pick is only ever a candidate: every drive is
# then named by its serial against the configured map. What this test pins:
#
#   - a substring of the model picks the enclosure's disks, the whole model too
#   - a pattern that is in no disk's model picks nothing (never "all")
#   - an empty or unset pattern lists every USB disk and no other transport
#   - a disk that is not on USB is never picked, whatever its model says
#
# The function is extracted from the script by `sed`; /sys/block is a fixture
# tree in a temp dir. VERIFY_SCRIPT points the test at another copy of the
# script (the falsification: the pre-yi8m file, which matched exactly).

set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFY="${VERIFY_SCRIPT:-$ROOT/scripts/backup-verify.sh}"
[[ -f "$VERIFY" ]] || { echo "FAIL: cannot find $VERIFY" >&2; exit 1; }
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK:?}"' EXIT

# --- fixture: three disks ----------------------------------------------------
# The transport test is "usb" anywhere in the resolved device path, so a temp
# dir whose random name held it would turn every disk into a USB one.
case "$WORK" in *usb*) echo "FAIL: temp dir $WORK contains 'usb'; run again" >&2; exit 1 ;; esac
mkdisk() { # name transport-dir model
    mkdir -p "$WORK/block/$1" "$WORK/devices/$2/$1"
    printf '%s\n' "$3" >"$WORK/devices/$2/$1/model"
    ln -s "$WORK/devices/$2/$1" "$WORK/block/$1/device"
}
mkdisk sda usb1 "Acme DAS-X6" # the enclosure's disk (model as sysfs pads it)
mkdisk sdb usb2 "Flash Stick" # another USB disk
mkdisk sdc pci0 "Acme DAS-X6" # same model, internal SATA: never a candidate

body="$(sed -n '/^detect_das_drives() {/,/^}/p' "$VERIFY")"
[[ -n "$body" ]] || { echo "FAIL: detect_das_drives not found in $VERIFY" >&2; exit 1; }
# Only the sysfs root moves to the fixture; the function is otherwise as shipped.
[[ "$body" == *'/sys/block/sd*'* ]] || { echo "FAIL: detect_das_drives no longer reads /sys/block/sd*" >&2; exit 1; }
body="${body//\/sys\/block\/sd\*/$WORK/block/sd*}"
eval "$body"

log_header() { :; }
log_error() { echo "ERROR: $*" >>"$WORK/log"; }
read_drive_serial() { echo "SERIAL-${1##*/}"; }
declare -A DRIVE_MAP=()

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

# result <pattern|UNSET>: the devices chosen and the exit status, on one line.
result() {
    : >"$WORK/log"
    local out rc=0
    out="$(
        DAS_DEVICES=()
        if [[ "$1" == UNSET ]]; then unset DAS_MODEL_PATTERN; else DAS_MODEL_PATTERN="$1"; fi
        detect_das_drives >/dev/null
        picked=()
        for d in "${DAS_DEVICES[@]}"; do picked+=("${d##*/}"); done
        (IFS=,; echo "${picked[*]}")
    )" || rc=$?
    echo "${out:+$out }status=$rc"
}

check "substring of the model picks the enclosure's disk" "$(result 'DAS-X6')" "sda status=0"
check "the whole model (blanks removed) picks it too" "$(result 'AcmeDAS-X6')" "sda status=0"
check "a substring of another USB disk's model picks that disk" "$(result 'Stick')" "sdb status=0"
check "a pattern in no disk's model picks nothing and fails" "$(result 'NoSuchEnclosure')" "status=1"
check "...and says the enclosure was not found, not 'all disks'" "$(grep -c 'No DAS drives detected' "$WORK/log")" "1"
check "an empty pattern lists every USB disk, no other transport" "$(result '')" "sda,sdb status=0"
check "an unset pattern is the same as empty" "$(result UNSET)" "sda,sdb status=0"

if [[ $fails -eq 0 ]]; then
    echo "VERIFY ENCLOSURE SUITE GREEN"
else
    echo "$fails FAILED"
    exit 1
fi
