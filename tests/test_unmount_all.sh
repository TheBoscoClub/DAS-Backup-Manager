#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: the arrays below are read by the functions extracted from
#   backup-run.sh, so their use is invisible here.
# SC2329: the umount/mountpoint/sleep/log stubs are called by that code.
#
# tests/test_unmount_all.sh
#
# unmount_all() and umount_with_retry() from scripts/backup-run.sh, driven by
# stub umount/mountpoint/sleep commands (bd DAS-Backup-Manager-5oc). Nothing
# here mounts or unmounts anything.
#
# Both directions:
#   - a target busy for a moment is released on a later attempt, and the run
#     records the unmount as OK;
#   - a target busy on every attempt is tried exactly UMOUNT_ATTEMPTS times,
#     and the run records the unmount as FAILED naming the mount point.
# Counter-check: with UMOUNT_ATTEMPTS=1 (no retry — the pre-4.6.1 behaviour)
# the "busy twice" target is recorded FAILED, so the retry is what makes the
# first case pass.

# The shell options backup-run.sh itself runs under: a function that let a
# failing command escape would abort the backup there, and must abort here.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in umount_with_retry unmount_all record_op; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done
eval "$(grep -E '^UMOUNT_(ATTEMPTS|RETRY_PAUSE)=' "$SCRIPT")"
[[ "${UMOUNT_ATTEMPTS:-}" == "5" ]] || { echo "FAIL: UMOUNT_ATTEMPTS not 5 in backup-run.sh"; exit 1; }

log_info() { :; }
log_warn() { echo "WARN: $*" >>"$WORK/log"; }
log_error() { echo "ERROR: $*" >>"$WORK/log"; }

# Stubs. A mount point is "mounted" while $WORK/mounted/<name> exists.
# $WORK/busy/<name> holds how many more umount calls fail with "target is busy".
mountpoint() { [[ -e "$WORK/mounted/$(basename "${!#}")" ]]; }
umount() {
    local name; name="$(basename "$1")"
    echo "$1" >>"$WORK/umount_calls"
    local left=0
    [[ -f "$WORK/busy/$name" ]] && left="$(cat "$WORK/busy/$name")"
    if (( left > 0 )); then
        echo $((left - 1)) >"$WORK/busy/$name"
        echo "umount: $1: target is busy." >&2
        return 32
    fi
    rm -f "$WORK/mounted/$name"
}
sleep() { echo "$1" >>"$WORK/sleeps"; }

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

setup() {
    rm -rf "$WORK/mounted" "$WORK/busy"
    mkdir -p "$WORK/mounted" "$WORK/busy"
    : >"$WORK/umount_calls"; : >"$WORK/sleeps"; : >"$WORK/log"
    declare -gA OP_STATUS=()
    declare -gA SOURCE_VOLUMES=()
    ALL_TARGET_MOUNTS=("/mnt/backup-22tb" "/mnt/backup-system-recovery-A")
    touch "$WORK/mounted/backup-22tb" "$WORK/mounted/backup-system-recovery-A"
}
calls() { grep -c "^$1\$" "$WORK/umount_calls" || true; }
lines() { wc -l <"$1" | tr -d ' '; }

# --- busy twice, then released ------------------------------------------------
setup
echo 2 >"$WORK/busy/backup-22tb"
unmount_all
check "busy twice: unmount recorded OK" "${OP_STATUS[unmount]}" "OK"
check "busy twice: three attempts on the busy target" "$(calls /mnt/backup-22tb)" "3"
check "busy twice: one attempt on the free target" "$(calls /mnt/backup-system-recovery-A)" "1"
check "busy twice: a pause after each failed attempt" "$(lines "$WORK/sleeps")" "2"
check "busy twice: the pause is the configured one" "$(sort -u "$WORK/sleeps")" "2"
check "busy twice: umount's own reason is logged" "$(grep -c 'target is busy' "$WORK/log")" "2"
check "busy twice: target really unmounted" "$([[ -e "$WORK/mounted/backup-22tb" ]] && echo yes || echo no)" "no"

# --- busy forever ---------------------------------------------------------------
setup
echo 99 >"$WORK/busy/backup-22tb"
unmount_all  # bare, as in main(): under set -e a non-zero return ends this test
check "busy forever: unmount recorded FAIL" "${OP_STATUS[unmount]}" "FAIL"
check "busy forever: the detail names the mount point" "${OP_STATUS[unmount_detail]}" "/mnt/backup-22tb"
check "busy forever: exactly five attempts" "$(calls /mnt/backup-22tb)" "5"
check "busy forever: no pause after the last attempt" "$(lines "$WORK/sleeps")" "4"
check "busy forever: the other target is still released" "$([[ -e "$WORK/mounted/backup-system-recovery-A" ]] && echo yes || echo no)" "no"
check "busy forever: the last attempt is an error" "$(grep -c '^ERROR:   umount /mnt/backup-22tb failed (attempt 5 of 5)' "$WORK/log")" "1"

# --- already unmounted: never touched -------------------------------------------
setup
rm -f "$WORK/mounted/backup-system-recovery-A"
unmount_all
check "not mounted: no umount call" "$(calls /mnt/backup-system-recovery-A)" "0"
check "not mounted: still OK" "${OP_STATUS[unmount]}" "OK"

# --- counter-check: without the retry, the transient case fails -----------------
setup
echo 2 >"$WORK/busy/backup-22tb"
UMOUNT_ATTEMPTS=1 unmount_all
check "no retry: the transient busy target is recorded FAIL" "${OP_STATUS[unmount]}" "FAIL"

if [[ $fails -eq 0 ]]; then
    echo "UNMOUNT RETRY SUITE GREEN"
else
    echo "$fails FAILED"
    exit 1
fi
