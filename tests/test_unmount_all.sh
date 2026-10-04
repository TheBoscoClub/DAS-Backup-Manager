#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: the arrays below are read by the functions extracted from
#   backup-run.sh, so their use is invisible here.
# SC2329: the mount/umount/mountpoint/sleep/log stubs are called by that code.
#
# tests/test_unmount_all.sh
#
# unmount_all() and umount_with_retry() from scripts/backup-run.sh, driven by
# stub umount/mountpoint/sleep commands (bd DAS-Backup-Manager-5oc). Nothing
# here mounts or unmounts anything.
#
# Targets, both directions:
#   - a target busy for a moment is released on a later attempt, and the run
#     records the unmount as OK;
#   - a target busy on every attempt is tried exactly UMOUNT_ATTEMPTS times,
#     and the run records the unmount as FAILED naming the mount point.
# Counter-check: with UMOUNT_ATTEMPTS=1 (no retry — the pre-4.6.1 behaviour)
# the "busy twice" target is recorded FAILED, so the retry is what makes the
# first case pass.
#
# Sources, with mount_sources() and a stub mount too: a run unmounts only the
# source mount points it mounted itself (bd DAS-Backup-Manager-8cf).
#   - One found mounted — as fstab mounts /dasRaid0 and the /.btrfs-* top
#     levels — is used as found and never unmounted, alone or shared by two
#     sources.
#   - One the run mounted is unmounted exactly once, however many sources
#     share it (nvme and nvme-vm share /.btrfs-nvme), last mounted first. A
#     second pass — cleanup() after an abort that follows main()'s unmount —
#     neither repeats it nor touches a mount someone else has made there since.
#   - A mount that failed is not the run's, even when the path is mounted by
#     the time cleanup() looks.
#   - A helper mount that will not unmount is a WARN carrying umount's own
#     message, not a FAIL, and stays the run's for a later pass.
# The found-mounted cases and the failed-mount case go red if unmount_all()
# unmounts every mounted source again, as it did before 4.11.2.

# The shell options backup-run.sh itself runs under: a function that let a
# failing command escape would abort the backup there, and must abort here.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in umount_with_retry unmount_all record_op mount_sources owns_source_mount disown_source_mount; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done
eval "$(grep -E '^UMOUNT_(ATTEMPTS|RETRY_PAUSE)=' "$SCRIPT")"
[[ "${UMOUNT_ATTEMPTS:-}" == "5" ]] || { echo "FAIL: UMOUNT_ATTEMPTS not 5 in backup-run.sh"; exit 1; }

log_info() { echo "INFO: $*" >>"$WORK/log"; }
log_warn() { echo "WARN: $*" >>"$WORK/log"; }
log_error() { echo "ERROR: $*" >>"$WORK/log"; }
abort_reason() { echo "ABORT: $1: $2" >>"$WORK/log"; }

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
# mount [options] <device> <mount point>. A mount point in MOUNT_RACES fails
# because something else mounted it first, as util-linux says it.
declare -A MOUNT_RACES=()
mount() {
    local mnt="${!#}"
    echo "$mnt" >>"$WORK/mount_calls"
    if [[ -n "${MOUNT_RACES[$mnt]:-}" ]]; then
        touch "$WORK/mounted/$(basename "$mnt")"
        echo "mount: $mnt: already mounted on $mnt." >&2
        return 32
    fi
    touch "$WORK/mounted/$(basename "$mnt")"
}
sleep() { echo "$1" >>"$WORK/sleeps"; }

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

setup() {
    rm -rf "$WORK/mounted" "$WORK/busy"
    mkdir -p "$WORK/mounted" "$WORK/busy"
    : >"$WORK/umount_calls"; : >"$WORK/mount_calls"; : >"$WORK/sleeps"; : >"$WORK/log"
    declare -gA OP_STATUS=()
    declare -gA SOURCE_VOLUMES=()
    declare -gA SOURCE_DEVICES=()
    SOURCE_MOUNTS_OWNED=()
    MOUNT_RACES=()
    ALL_TARGET_MOUNTS=("/mnt/backup-22tb" "/mnt/backup-system-recovery-A")
    touch "$WORK/mounted/backup-22tb" "$WORK/mounted/backup-system-recovery-A"
}
# The run's sources, as label=mount point pairs.
sources() {
    local pair
    for pair in "$@"; do
        SOURCE_VOLUMES[${pair%%=*}]="${pair#*=}"
        SOURCE_DEVICES[${pair%%=*}]="UUID=${pair%%=*}-uuid"
    done
}
# Mounted before the run looks, as fstab mounts them at boot.
premounted() { local m; for m in "$@"; do touch "$WORK/mounted/$(basename "$m")"; done; }
is_mounted() { [[ -e "$WORK/mounted/$(basename "$1")" ]] && echo yes || echo no; }
calls() { grep -cxF -- "$1" "$WORK/umount_calls" || true; }
mounts() { grep -cxF -- "$1" "$WORK/mount_calls" || true; }
logged() { grep -cxF -- "$1" "$WORK/log" || true; }
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

# --- a source found mounted is never the run's ----------------------------------
setup
sources das-storage=/dasRaid0
premounted /dasRaid0
mount_sources
check "found mounted: not mounted again" "$(mounts /dasRaid0)" "0"
check "found mounted: says it is used as found" \
    "$(logged 'INFO:   das-storage: /dasRaid0 was already mounted — used as found; this run will not unmount it')" "1"
unmount_all
check "found mounted: never unmounted" "$(calls /dasRaid0)" "0"
check "found mounted: still mounted" "$(is_mounted /dasRaid0)" "yes"
check "found mounted: the targets are still released" \
    "$(calls /mnt/backup-22tb) $(calls /mnt/backup-system-recovery-A)" "1 1"
check "found mounted: unmount recorded OK" "${OP_STATUS[unmount]}" "OK"

setup
sources nvme=/.btrfs-nvme nvme-vm=/.btrfs-nvme
premounted /.btrfs-nvme
mount_sources
unmount_all
check "found mounted, shared by two sources: neither mounted nor unmounted" \
    "$(mounts /.btrfs-nvme) $(calls /.btrfs-nvme) $(is_mounted /.btrfs-nvme)" "0 0 yes"

# --- a source the run mounted is unmounted exactly once ---------------------------
setup
sources hdd-media=/.btrfs-hdd
mount_sources
check "mounted by the run: mounted once" "$(mounts /.btrfs-hdd)" "1"
unmount_all
check "mounted by the run: unmounted exactly once" "$(calls /.btrfs-hdd)" "1"
check "mounted by the run: really unmounted" "$(is_mounted /.btrfs-hdd)" "no"
check "mounted by the run: says so" \
    "$(logged 'INFO:   Unmounted source volume /.btrfs-hdd (this run mounted it)')" "1"

setup
sources nvme=/.btrfs-nvme nvme-vm=/.btrfs-nvme
mount_sources
unmount_all
check "mounted by the run, shared by two sources: one mount, one umount" \
    "$(mounts /.btrfs-nvme) $(calls /.btrfs-nvme) $(is_mounted /.btrfs-nvme)" "1 1 no"
check "mounted by the run, shared by two sources: the second is not 'found mounted'" \
    "$(grep -c 'used as found' "$WORK/log" || true)" "0"

# The live host's layout: /dasRaid0 is fstab's; the three top levels are
# mounted by the run (as on a night after the old cleanup took them down).
setup
sources nvme=/.btrfs-nvme nvme-vm=/.btrfs-nvme ssd=/.btrfs-ssd \
    hdd-media=/.btrfs-hdd hdd-system=/.btrfs-hdd das-storage=/dasRaid0
premounted /dasRaid0
mount_sources
check "live layout: three helper mounts" "$(lines "$WORK/mount_calls")" "3"
unmount_all
check "live layout: /dasRaid0 never unmounted, still mounted" "$(calls /dasRaid0) $(is_mounted /dasRaid0)" "0 yes"
check "live layout: each helper mount unmounted once" \
    "$(calls /.btrfs-nvme) $(calls /.btrfs-ssd) $(calls /.btrfs-hdd)" "1 1 1"
check "live layout: last mounted, first unmounted" \
    "$(grep -vxF -e /mnt/backup-22tb -e /mnt/backup-system-recovery-A "$WORK/umount_calls" | tr '\n' ' ')" \
    "$(tac "$WORK/mount_calls" | tr '\n' ' ')"

# A second pass — cleanup() after an abort that follows main()'s own
# unmount — does not unmount again ...
setup
sources hdd-media=/.btrfs-hdd
mount_sources
unmount_all
unmount_all
check "second pass: still one umount" "$(calls /.btrfs-hdd)" "1"

# ... nor take down a mount someone else made there in between.
setup
sources hdd-media=/.btrfs-hdd
mount_sources
unmount_all
premounted /.btrfs-hdd
unmount_all
check "mounted again by someone else: left alone by the second pass" \
    "$(calls /.btrfs-hdd) $(is_mounted /.btrfs-hdd)" "1 yes"

# --- a source whose mount failed is not the run's ----------------------------------
# mount_sources() stops the run (exit 3) and cleanup() unmounts: here the EXIT
# trap of a subshell. Something else mounted the path first, so it IS mounted
# when cleanup() looks — and must be left alone.
setup
sources ssd=/.btrfs-ssd
MOUNT_RACES["/.btrfs-ssd"]=1
rc=0
(trap unmount_all EXIT; mount_sources) || rc=$?
check "mount failed: the run stops with 3" "$rc" "3"
check "mount failed, mounted by something else meanwhile: never unmounted" \
    "$(calls /.btrfs-ssd) $(is_mounted /.btrfs-ssd)" "0 yes"

# One mounted before another failed is unmounted, once; the failed one never.
# bash's hash order visits hdd-media before ssd; the first check says so, so
# a bash that orders them otherwise fails here rather than passing on 0 = 0.
setup
sources hdd-media=/.btrfs-hdd ssd=/.btrfs-ssd
MOUNT_RACES["/.btrfs-ssd"]=1
rc=0
(trap unmount_all EXIT; mount_sources) || rc=$?
check "mount failed after another: the run stops with 3" "$rc" "3"
check "mount failed after another: the one mounted first is unmounted once" \
    "$(mounts /.btrfs-hdd) $(calls /.btrfs-hdd)" "1 1"
check "mount failed after another: the failed one never unmounted" \
    "$(calls /.btrfs-ssd) $(is_mounted /.btrfs-ssd)" "0 yes"

# --- a helper mount that will not unmount ------------------------------------------
setup
sources hdd-media=/.btrfs-hdd
mount_sources
echo 1 >"$WORK/busy/.btrfs-hdd"
unmount_all
check "helper busy: one attempt, best effort" "$(calls /.btrfs-hdd)" "1"
check "helper busy: a WARN carrying umount's own message" \
    "$(logged 'WARN:   Could not unmount source volume /.btrfs-hdd, which this run mounted: umount: /.btrfs-hdd: target is busy. — left mounted; best effort, not a DAS disconnect concern')" "1"
check "helper busy: not a FAIL" "${OP_STATUS[unmount]}" "OK"
check "helper busy: no ERROR line" "$(grep -c '^ERROR:' "$WORK/log" || true)" "0"
check "helper busy: left mounted" "$(is_mounted /.btrfs-hdd)" "yes"
unmount_all
check "helper busy: still the run's, so a later pass releases it" \
    "$(calls /.btrfs-hdd) $(is_mounted /.btrfs-hdd)" "2 no"

if [[ $fails -eq 0 ]]; then
    echo "UNMOUNT RETRY SUITE GREEN"
else
    echo "$fails FAILED"
    exit 1
fi
