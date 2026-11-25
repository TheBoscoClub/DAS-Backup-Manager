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
# Absent before the su1b.4 UUID enumeration: its absence is the red state.
eval "$(sed -n '/^uuid_member_disks() {/,/^}/p' "$SCRIPT")"

# A two-member RAID1 by filesystem UUID, found by a stub blkid on an env -i
# style PATH (the stub never calls its own name), and a fake sysfs.
SCRATCH="$(mktemp -d)"
trap 'rm -rf "${SCRATCH:?}"' EXIT
mkdir -p "$SCRATCH/bin" "$SCRATCH/sys/devices/sdi/sdi1" "$SCRATCH/sys/devices/sdj/sdj1" "$SCRATCH/sys/devices/sdn"
touch "$SCRATCH/sys/devices/sdi/sdi1/partition" "$SCRATCH/sys/devices/sdj/sdj1/partition"
mkdir -p "$SCRATCH/sys/class"
ln -s ../devices/sdi/sdi1 "$SCRATCH/sys/class/sdi1"
ln -s ../devices/sdj/sdj1 "$SCRATCH/sys/class/sdj1"
ln -s ../devices/sdn "$SCRATCH/sys/class/sdn"
cat >"$SCRATCH/bin/blkid" <<'STUB'
#!/bin/bash
# args: -c /dev/null -t UUID=<u> -o device
case "$*" in
    *UUID=two-members*) printf '/dev/sdi1
/dev/sdj1
' ;;
    *UUID=whole-disk*) printf '/dev/sdn
/dev/sdj1
' ;;
    *UUID=broken*) echo "blkid: boom" >&2; exit 4 ;;
    *UUID=ghost*) printf '/dev/sdz1
' ;;
    *) exit 2 ;;
esac
STUB
chmod +x "$SCRATCH/bin/blkid"
export PATH="$SCRATCH/bin:$PATH" DAS_SYS_CLASS_BLOCK="$SCRATCH/sys/class"
log_warn() { printf 'WARN %s\n' "$1"; }

declare -A DISCOVERED_DEVICES TARGET_SERIAL_DEVS TARGET_MOUNT_UUIDS
DISCOVERED_DEVICES[primary]=/dev/sdi
TARGET_SERIAL_DEVS[primary]="SERIAL11=/dev/sdi SERIAL12=/dev/sdj"
DISCOVERED_DEVICES[recovery-a]=/dev/sdk
TARGET_SERIAL_DEVS[recovery-a]="SERIAL04=/dev/sdk"
DISCOVERED_DEVICES[half]=""
TARGET_SERIAL_DEVS[half]="SERIAL21= SERIAL22=/dev/sdm"

check "a RAID1 pair: both legs, once each" "$(io_scheduler_devices primary | tr '\n' ' ')" "/dev/sdi /dev/sdj "
check "one drive: that drive" "$(io_scheduler_devices recovery-a | tr '\n' ' ')" "/dev/sdk "
check "a leg not found by serial and no UUID disk: only the found leg" "$(io_scheduler_devices half | tr '\n' ' ')" "/dev/sdm "
# No serial map, first device only: the UUID alone must reach both legs.
DISCOVERED_DEVICES[raid]=/dev/sdi
TARGET_MOUNT_UUIDS[raid]=two-members
check "UUID with two members, serials unknown: both disks, once each" "$(io_scheduler_devices raid 2>&1 | tr '\n' ' ')" "/dev/sdi /dev/sdj "
DISCOVERED_DEVICES[whole]=""
TARGET_MOUNT_UUIDS[whole]=whole-disk
check "a whole-disk member and a partition member" "$(io_scheduler_devices whole 2>&1 | tr '\n' ' ')" "/dev/sdn /dev/sdj "
DISCOVERED_DEVICES[broken]=/dev/sdi
TARGET_MOUNT_UUIDS[broken]=broken
check "blkid failing: the known disk, and a WARN naming the failure" "$(io_scheduler_devices broken 2>&1 | tr '\n' '|')" "WARN   blkid could not list the members of filesystem broken (status 4): blkid: boom|/dev/sdi|"
DISCOVERED_DEVICES[ghost]=""
TARGET_MOUNT_UUIDS[ghost]=ghost
check "a member with no sysfs entry: a WARN, never silent" "$(io_scheduler_devices ghost 2>&1 | tr '\n' '|')" "WARN   /dev/sdz1 carries filesystem ghost but $SCRATCH/sys/class/sdz1 is missing -- its I/O scheduler is not set|"
check "a label with nothing: nothing" "$(io_scheduler_devices nosuch | tr '\n' ' ')" ""

if ((fails == 0)); then
    echo "IO SCHEDULER SUITE GREEN"
else
    echo "IO SCHEDULER SUITE RED ($fails failed)"
    exit 1
fi
