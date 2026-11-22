#!/bin/bash
# shellcheck disable=SC2034 # the arrays are read by the extracted io_scheduler_devices
# The I/O scheduler reaches every disk of a multi-drive target (a RAID1 pair's
# two legs), not only the one disk blkid resolves the filesystem UUID to.
set -uo pipefail
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
fails=0
check() {
    if [[ "$2" == "$3" ]]; then
        echo "ok   $1"
    else
        echo "FAIL $1: got '$2', want '$3'"
        fails=$((fails + 1))
    fi
}

body="$(sed -n '/^io_scheduler_devices() {/,/^}/p' "$SCRIPT")"
if [[ -z "$body" ]]; then
    echo "FAIL io_scheduler_devices() not found in $SCRIPT"
    echo "IO SCHEDULER SUITE RED"
    exit 1
fi
eval "$body"

declare -A DISCOVERED_DEVICES TARGET_SERIAL_DEVS
DISCOVERED_DEVICES[primary]=/dev/sdi
TARGET_SERIAL_DEVS[primary]="SERIAL11=/dev/sdi SERIAL12=/dev/sdj"
DISCOVERED_DEVICES[recovery-a]=/dev/sdk
TARGET_SERIAL_DEVS[recovery-a]="SERIAL04=/dev/sdk"
DISCOVERED_DEVICES[half]=""
TARGET_SERIAL_DEVS[half]="SERIAL21= SERIAL22=/dev/sdm"

check "a RAID1 pair: both legs, once each" "$(io_scheduler_devices primary | tr '\n' ' ')" "/dev/sdi /dev/sdj "
check "one drive: that drive" "$(io_scheduler_devices recovery-a | tr '\n' ' ')" "/dev/sdk "
check "a leg not found by serial and no UUID disk: only the found leg" "$(io_scheduler_devices half | tr '\n' ' ')" "/dev/sdm "
check "a label with nothing: nothing" "$(io_scheduler_devices nosuch | tr '\n' ' ')" ""

if ((fails == 0)); then
    echo "IO SCHEDULER SUITE GREEN"
else
    echo "IO SCHEDULER SUITE RED ($fails failed)"
    exit 1
fi
