#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: the run state set here is read by log_throughput, extracted from
#   backup-run.sh, so the use site is invisible to shellcheck.
# SC2329: the stubs are called by that same extracted code.
#
# tests/test_usb_link.sh
#
# The USB link-speed probe in scripts/backup-run.sh's log_throughput(): the
# enclosure is the USB device whose product string contains
# [das].model_pattern (DAS_MODEL_PATTERN). No pattern has no built-in
# enclosure: the probe singles out no device, says why, and records the link as
# "unknown" — never the first USB device's speed. A configured pattern finds
# the enclosure among other devices, and a slow link still fails the usb_link
# operation. /sys/bus/usb/devices is a fixture tree here.
set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK:?}"' EXIT
FAILS=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; FAILS=$((FAILS + 1)); fi; }
has() { if grep -qF -- "$3" <<<"$2"; then echo "ok   $1"; else echo "FAIL $1: no '$3' in: $2"; FAILS=$((FAILS + 1)); fi; }
lacks() { if grep -qF -- "$3" <<<"$2"; then echo "FAIL $1: '$3' in: $2"; FAILS=$((FAILS + 1)); else echo "ok   $1"; fi; }

body="$(sed -n '/^log_throughput() {/,/^}/p' "$SCRIPT")"
[[ -n "$body" ]] || { echo "FAIL: log_throughput not found in backup-run.sh"; exit 1; }
grep -qF '/sys/bus/usb/devices/*/' <<<"$body" || { echo "FAIL: log_throughput no longer reads /sys/bus/usb/devices"; exit 1; }
eval "${body//\/sys\/bus\/usb\/devices\//$WORK\/usb\/}"

log_info() { echo "INFO: $*" >>"$WORK/log"; }
log_warn() { echo "WARN: $*" >>"$WORK/log"; }
record_op() { echo "$*" >>"$WORK/ops"; }
format_bytes() { echo "$1 B"; }

device() { # <name> <product> <speed>
    mkdir -p "$WORK/usb/$1"
    printf '%s\n' "$2" >"$WORK/usb/$1/product"
    printf '%s\n' "$3" >"$WORK/usb/$1/speed"
}
run() {
    : >"$WORK/log"; : >"$WORK/ops"; : >"$WORK/tp.jsonl"
    BTRBK_START_TIME=0 BTRBK_END_TIME=100
    ALL_TARGET_MOUNTS=(/mnt/backup-primary)
    declare -gA TARGET_NAMES=([/mnt/backup-primary]=primary)
    declare -gA USAGE_BEFORE=([/mnt/backup-primary]=1000)
    declare -gA USAGE_AFTER=([/mnt/backup-primary]=2000)
    DAS_THROUGHPUT_LOG="$WORK/tp.jsonl"
    log_throughput
}

# A keyboard first on the bus, at full speed (12 Mbit/s), then the enclosure.
device 1-1 "USB Keyboard" 12
device 2-1 "Example DAS Enclosure" 10000

unset DAS_EXPECTED_LINK_MBIT_S
DAS_MODEL_PATTERN=""
run
has "no pattern: says the link was not checked and why" "$(cat "$WORK/log")" "no [das].model_pattern is configured"
has "no pattern: the link is recorded unknown" "$(cat "$WORK/tp.jsonl")" '"usb_link_mbit_s":"unknown"'
lacks "no pattern: never the keyboard's speed" "$(cat "$WORK/tp.jsonl")" '"usb_link_mbit_s":"12"'
check "no pattern: no usb_link row (the report shows N/A)" "$(cat "$WORK/ops")" ""

DAS_MODEL_PATTERN="DAS Enclosure"
run
has "pattern: the enclosure's link is recorded" "$(cat "$WORK/tp.jsonl")" '"usb_link_mbit_s":"10000"'
has "pattern, link at 10000: usb_link OK" "$(cat "$WORK/ops")" "usb_link OK negotiated 10000 Mbit/s"
lacks "pattern, link at 10000: no usb_link FAIL" "$(cat "$WORK/ops")" "FAIL"
lacks "pattern: no not-checked note" "$(cat "$WORK/log")" "not checked"

printf '480\n' >"$WORK/usb/2-1/speed"
run
has "pattern, link at 480: recorded" "$(cat "$WORK/tp.jsonl")" '"usb_link_mbit_s":"480"'
has "pattern, link at 480: usb_link FAIL" "$(cat "$WORK/ops")" "usb_link FAIL negotiated 480 Mbit/s, expected 10000"

# USB 3.2 Gen1 is half the rated Gen2 link: a downgrade, not a pass (bd 7qe).
printf '5000\n' >"$WORK/usb/2-1/speed"
run
has "link at 5000 of an expected 10000: usb_link FAIL" "$(cat "$WORK/ops")" "usb_link FAIL negotiated 5000 Mbit/s, expected 10000"

# The expected rate is configuration ([das].expected_link_mbit_s), never a
# built-in enclosure: a Gen1 enclosure at its own rate passes.
DAS_EXPECTED_LINK_MBIT_S=5000
run
has "expected 5000, link at 5000: usb_link OK" "$(cat "$WORK/ops")" "usb_link OK negotiated 5000 Mbit/s"
unset DAS_EXPECTED_LINK_MBIT_S

# Every matching device is read, not only the first: the slow one fails it.
printf '10000\n' >"$WORK/usb/2-1/speed"
device 3-1 "Example DAS Enclosure" 480
run
has "two enclosure devices, one at 480: usb_link FAIL" "$(cat "$WORK/ops")" "usb_link FAIL negotiated 480 Mbit/s, expected 10000"
has "two enclosure devices: the slowest is recorded" "$(cat "$WORK/tp.jsonl")" '"usb_link_mbit_s":"480"'

# A run that wrote nothing still checks the bus: link health is not traffic.
run_nothing_written() {
    : >"$WORK/log"; : >"$WORK/ops"; : >"$WORK/tp.jsonl"
    BTRBK_START_TIME=0 BTRBK_END_TIME=100
    ALL_TARGET_MOUNTS=(/mnt/backup-primary)
    declare -gA TARGET_NAMES=([/mnt/backup-primary]=primary)
    declare -gA USAGE_BEFORE=([/mnt/backup-primary]=2000)
    declare -gA USAGE_AFTER=([/mnt/backup-primary]=2000)
    DAS_THROUGHPUT_LOG="$WORK/tp.jsonl"
    log_throughput
}
run_nothing_written
has "nothing written, a device at 480: usb_link FAIL" "$(cat "$WORK/ops")" "usb_link FAIL negotiated 480 Mbit/s"
rm -r "${WORK:?}/usb/3-1"

DAS_MODEL_PATTERN="Nothing Like It"
run
has "a pattern no device matches: unknown" "$(cat "$WORK/tp.jsonl")" '"usb_link_mbit_s":"unknown"'
has "a pattern no device matches: usb_link WARN, naming the pattern" "$(cat "$WORK/ops")" "usb_link WARN no USB device's product contains 'Nothing Like It'"

# The report carries the row, so a FAIL says why in the email, not only the journal.
# shellcheck disable=SC2016 # matched as written
has "the report has a USB link row" "$(cat "$SCRIPT")" 'USB link              ${OP_STATUS[usb_link]:-N/A}'

echo
if ((FAILS > 0)); then printf '%d check(s) FAILED\n' "$FAILS"; exit 1; fi
echo "USB-LINK SUITE GREEN"
