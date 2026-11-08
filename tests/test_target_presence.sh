#!/bin/bash
# check_das_connected / the per-target report rows / generate_smart_section
# from scripts/backup-run.sh (bd DAS-Backup-Manager-uhi). A missing leg of a
# RAID-1 target and an absent recovery target each get a row in BACKUP
# OPERATIONS naming the drive, and SMART STATUS has one row per configured
# serial, an absent one shown as ABSENT, never left out.
# Both directions: every drive present -> an OK row per target, ALL OPERATIONS
# SUCCESSFUL, no ABSENT; a leg missing -> WARN and COMPLETED WITH WARNINGS; a
# recovery target missing -> FAIL and FAILURES DETECTED; found only by its
# filesystem UUID -> OK, the row naming the serials not seen.
set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK:?}"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in check_das_connected record_op any_op_is generate_report generate_smart_section smart_row; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done

# shellcheck disable=SC2329  # called by the eval-extracted functions
log_info() { echo "INFO: $*" >>"$WORK/log"; }; log_warn() { echo "WARN: $*" >>"$WORK/log"; }; log_error() { echo "ERROR: $*" >>"$WORK/log"; }
# shellcheck disable=SC2329
abort_reason() { echo "ABORT: $*" >>"$WORK/log"; }

# Drives present on the "host": $WORK/present holds "SERIAL device" lines.
# shellcheck disable=SC2329  # called by the extracted check_das_connected
find_device_by_serial() { awk -v s="$1" '$1 == s { print $2; f = 1 } END { exit !f }' "$WORK/present"; }
# A filesystem UUID resolves only when $WORK/uuid_dev names the device.
# shellcheck disable=SC2329
blkid() { [[ -s "$WORK/uuid_dev" && "$1" == -U ]] || return 2; cat "$WORK/uuid_dev"; }
# shellcheck disable=SC2329
parent_disk() { echo "$1"; }
# shellcheck disable=SC2329  # called by the extracted generate_smart_section
get_smart_summary() { echo "PASSED|31|1200|from-smartctl"; }

# generate_report's other sections and host probe are stubbed; only the status
# line, the target rows and SMART STATUS are under test.
systemctl() { :; }
generate_throughput_section() { echo "  tp"; }
generate_capacity_section() { echo "  cap"; }
generate_growth_section() { echo "  growth"; }
next_scheduled() { echo "n/a"; }
# shellcheck disable=SC2034  # read by the extracted generate_report
BTRBK_START_TIME=0 BTRBK_END_TIME=0 BTRBK_LATEST="" SUBVOL_SYNC_REPORT="" SUBVOL_EXPIRE_REPORT="" RECOVERY_OS_REPORT=""

# Two targets: a RAID-1 primary of two drives and a one-drive recovery mirror.
# shellcheck disable=SC2034  # read by the extracted functions
declare -A DAS_SERIALS=([primary]=SERIAL11 [recov-a]=SERIAL04)
# shellcheck disable=SC2034
declare -A DAS_SERIALS_LIST=([primary]="SERIAL11 SERIAL12" [recov-a]=SERIAL04)
# shellcheck disable=SC2034
declare -A TARGET_MOUNT_UUIDS=([primary]=5eed0016-0000-4000-8000-000000000016 [recov-a]=5eed0008-0000-4000-8000-000000000008)
# shellcheck disable=SC2034
declare -A TARGET_ROLES=([primary]=primary [recov-a]=mirror)
# shellcheck disable=SC2034
declare -A TARGET_MOUNTS=([primary]=/mnt/p [recov-a]=/mnt/a)
# shellcheck disable=SC2034
declare -A TARGET_NAMES=([/mnt/p]=primary [/mnt/a]=recov-a)

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

run_case() { # run_case <present lines>
    declare -gA OP_STATUS=([btrbk]=OK)
    : >"$WORK/log"
    printf '%s' "$1" >"$WORK/present"
    check_das_connected
    report="$(generate_report)"
    smart="$(sed -n '/^SMART STATUS$/,/^LATEST SNAPSHOTS$/p' <<<"$report")"
}

# --- every drive present ------------------------------------------------------
run_case $'SERIAL11 /dev/sdb\nSERIAL12 /dev/sdc\nSERIAL04 /dev/sdd\n'
check "all present: status" "$(grep -c '^  Status: ALL OPERATIONS SUCCESSFUL$' <<<"$report")" "1"
check "all present: primary row" "$(grep -c '^  Target primary  *OK  (SERIAL11 /dev/sdb, SERIAL12 /dev/sdc)$' <<<"$report")" "1"
check "all present: recov-a row" "$(grep -c '^  Target recov-a  *OK  (SERIAL04 /dev/sdd)$' <<<"$report")" "1"
check "all present: a SMART row per drive" "$(grep -cE '^  (primary|recov-a) +SERIAL(11|12|04) +PASSED ' <<<"$smart")" "3"
check "all present: no ABSENT" "$(grep -c ABSENT <<<"$report" || true)" "0"

# --- one leg of the RAID-1 pair missing ---------------------------------------
run_case $'SERIAL11 /dev/sdb\nSERIAL04 /dev/sdd\n'
check "leg missing: status" "$(grep -c '^  Status: COMPLETED WITH WARNINGS$' <<<"$report")" "1"
check "leg missing: primary row names the drive" \
    "$(grep -c '^  Target primary  *WARN  (RAID-1 degraded: SERIAL12 absent; running on SERIAL11 /dev/sdb)$' <<<"$report")" "1"
check "leg missing: the present leg's SMART row" "$(grep -cE '^  primary +SERIAL11 +PASSED ' <<<"$smart")" "1"
check "leg missing: the absent leg's SMART row" "$(grep -cE '^  primary +SERIAL12 +ABSENT$' <<<"$smart")" "1"
check "leg missing: still mounts the target" "${TARGET_AVAILABLE[primary]}" "true"

# --- the recovery target missing ----------------------------------------------
run_case $'SERIAL11 /dev/sdb\nSERIAL12 /dev/sdc\n'
check "target missing: status" "$(grep -c '^  Status: FAILURES DETECTED$' <<<"$report")" "1"
check "target missing: its row" \
    "$(grep -c '^  Target recov-a  *FAIL  (unavailable: SERIAL04 absent and no mountable UUID)$' <<<"$report")" "1"
check "target missing: its SMART row" "$(grep -cE '^  recov-a +SERIAL04 +ABSENT$' <<<"$smart")" "1"
check "target missing: the primary is fine" "$(grep -c '^  Target primary  *OK  (' <<<"$report")" "1"

# --- the primary found only by its filesystem UUID ----------------------------
echo /dev/sdx >"$WORK/uuid_dev"
run_case $'SERIAL04 /dev/sdd\n'
rm -f "${WORK:?}/uuid_dev"
check "UUID only: status" "$(grep -c '^  Status: ALL OPERATIONS SUCCESSFUL$' <<<"$report")" "1"
check "UUID only: primary row says what was not seen" \
    "$(grep -c '^  Target primary  *OK  (found by UUID on /dev/sdx; serial(s) not seen: SERIAL11 SERIAL12)$' <<<"$report")" "1"
check "UUID only: a SMART row for the disk under it" "$(grep -cE '^  primary +from-smartctl +PASSED ' <<<"$smart")" "1"
check "UUID only: the configured serials shown absent" "$(grep -cE '^  primary +SERIAL1[12] +ABSENT$' <<<"$smart")" "2"

# --- a run from before detection: no target rows, no SMART rows ---------------
# shellcheck disable=SC2034  # read by the extracted generate_report
declare -gA OP_STATUS=([btrbk]=OK)
unset TARGET_SERIAL_DEVS TARGET_AVAILABLE DISCOVERED_DEVICES
report="$(generate_report)"
check "before detection: no target rows" "$(grep -c '^  Target ' <<<"$report" || true)" "0"
check "before detection: no SMART rows" "$(sed -n '/^SMART STATUS$/,/^LATEST SNAPSHOTS$/p' <<<"$report" | grep -c SERIAL || true)" "0"

if ((fails > 0)); then
    echo "test_target_presence: $fails FAILED"
    exit 1
fi
echo "test_target_presence: all passed"
