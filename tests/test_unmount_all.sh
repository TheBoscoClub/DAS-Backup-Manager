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
# A target the probe cannot tell about (bd DAS-Backup-Manager-jug6) fails the
# gate whether or not the unmount tried anyway succeeds, and the detail says
# why; one whose path is not there at all (an absent drive's, removed on
# purpose) is not a failure.
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
#   - The probe's three answers (8cf review, F1): mountpoint answers as
#     util-linux does (0, 32, 1). "Could not tell" is said, with the probe's
#     message, and the helper is unmounted anyway; "not mounted", or a path
#     that is not there, is struck off and said, never unmounted.
#   - A mount point fstab declares, found unmounted, is said once, as INFO.
# The found-mounted cases and the failed-mount case go red if unmount_all()
# unmounts every mounted source again, as it did before 4.11.2. A stop while
# mount runs needs a signal, so test_backup_exit_semantics.sh holds that one.

# The shell options backup-run.sh itself runs under: a function that let a
# failing command escape would abort the backup there, and must abort here.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in umount_with_retry unmount_all record_op mount_sources owns_source_mount disown_source_mount probe_mount_point probe_state; do
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
# mountpoint answers as util-linux 2.42.4 does (measured): 0 a mount point, 32
# not one, 1 an error — and 1, "No such file or directory", for a path that
# does not exist. Every path here exists but those in $WORK/absent; one in
# $WORK/probe_errors answers an error.
mountpoint() {
    local p="${!#}" name
    name="$(basename "$p")"
    if [[ -e "$WORK/probe_errors/$name" ]]; then
        echo "mountpoint: $p: Input/output error" >&2
        return 1
    fi
    # An exit status of its own, with nothing said ($WORK/probe_rc/<name> holds
    # it), and an error that takes two lines ($WORK/probe_two/<name>).
    if [[ -e "$WORK/probe_rc/$name" ]]; then
        return "$(cat "$WORK/probe_rc/$name")"
    fi
    if [[ -e "$WORK/probe_two/$name" ]]; then
        echo "mountpoint: first line" >&2
        echo "second line" >&2
        return 1
    fi
    # An error whose message ends the way the helper's own status line does.
    if [[ -e "$WORK/probe_fake/$name" ]]; then
        echo "status=0" >&2
        return 1
    fi
    if [[ -e "$WORK/absent/$name" ]]; then
        echo "mountpoint: $p: No such file or directory" >&2
        return 1
    fi
    [[ -e "$WORK/mounted/$name" ]] || return 32
}
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
    if [[ ! -e "$WORK/mounted/$name" ]]; then
        echo "umount: $1: not mounted." >&2
        return 32
    fi
    rm -f "$WORK/mounted/$name"
}
# findmnt --fstab ... --mountpoint <path>: declared while $WORK/fstab/<name> exists.
findmnt() {
    [[ " $* " == *" --fstab "* ]] || return 1
    [[ -e "$WORK/fstab/$(basename "${!#}")" ]]
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
    rm -rf "$WORK/mounted" "$WORK/busy" "$WORK/probe_errors" "$WORK/absent" "$WORK/fstab" "$WORK/probe_rc" "$WORK/probe_two" "$WORK/probe_fake"
    mkdir -p "$WORK/mounted" "$WORK/busy" "$WORK/probe_errors" "$WORK/absent" "$WORK/fstab" "$WORK/probe_rc" "$WORK/probe_two" "$WORK/probe_fake"
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
check "busy forever: the detail names the mount point" "${OP_STATUS[unmount_detail]}" "still mounted: /mnt/backup-22tb"
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

# --- the probe's three answers, for a target (bd DAS-Backup-Manager-jug6) -----
# A probe error used to read as "not mounted": the target was skipped, the
# unmount recorded OK, and the run said the DAS could be disconnected while a
# drive was mounted. "Could not tell" now fails the gate and says why — what
# the probe said and what umount did — and the unmount is tried anyway.
setup
touch "$WORK/probe_errors/backup-22tb"
unmount_all
check "target, probe cannot tell, mounted: unmounted anyway" \
    "$(calls /mnt/backup-22tb) $(is_mounted /mnt/backup-22tb)" "1 no"
check "target, probe cannot tell, mounted: the gate fails all the same" "${OP_STATUS[unmount]}" "FAIL"
check "target, probe cannot tell, mounted: the detail says why, and what umount did" \
    "${OP_STATUS[unmount_detail]:-<none>}" \
    "could not tell whether /mnt/backup-22tb is mounted — mountpoint: /mnt/backup-22tb: Input/output error (exit 1); umount then succeeded"
check "target, probe cannot tell, mounted: said as an error, with the probe's message" \
    "$(logged 'ERROR:   Could not tell whether /mnt/backup-22tb is mounted — mountpoint: /mnt/backup-22tb: Input/output error (exit 1); unmounting it anyway')" "1"
check "target, probe cannot tell, mounted: the other target released as before" \
    "$(calls /mnt/backup-system-recovery-A) $(is_mounted /mnt/backup-system-recovery-A)" "1 no"

setup
rm -f "$WORK/mounted/backup-22tb"
touch "$WORK/probe_errors/backup-22tb"
unmount_all
check "target, probe cannot tell, not mounted: the unmount tried with its whole budget" \
    "$(calls /mnt/backup-22tb)" "5"
check "target, probe cannot tell, not mounted: the gate still fails" "${OP_STATUS[unmount]}" "FAIL"
check "target, probe cannot tell, not mounted: the detail says umount failed too" \
    "${OP_STATUS[unmount_detail]:-<none>}" \
    "could not tell whether /mnt/backup-22tb is mounted — mountpoint: /mnt/backup-22tb: Input/output error (exit 1); umount failed too"

# Both kinds at once: each is in the detail, the one still mounted first.
setup
echo 99 >"$WORK/busy/backup-22tb"
touch "$WORK/probe_errors/backup-system-recovery-A"
unmount_all
check "a target still mounted and one the probe cannot tell about: both in the detail" \
    "${OP_STATUS[unmount_detail]:-<none>}" \
    "still mounted: /mnt/backup-22tb; could not tell whether /mnt/backup-system-recovery-A is mounted — mountpoint: /mnt/backup-system-recovery-A: Input/output error (exit 1); umount then succeeded"

# An absent drive's mount point is removed on purpose (create_mount_points):
# mountpoint answers it 1, "No such file or directory", like an error, and it
# is NOT one — the gate passes, and nothing is said about it.
setup
rm -f "$WORK/mounted/backup-system-recovery-A"
touch "$WORK/absent/backup-system-recovery-A"
unmount_all
check "target not there at all (an absent drive): never unmounted" "$(calls /mnt/backup-system-recovery-A)" "0"
check "target not there at all (an absent drive): the gate passes" "${OP_STATUS[unmount]}" "OK"
check "target not there at all (an absent drive): nothing said about it" \
    "$(grep -c 'backup-system-recovery-A' "$WORK/log" || true)" "0"

# --- the probe's three answers, for the run's own helper (8cf review, F1) ------
# mountpoint's error is not "not mounted": the helper was struck off and left
# mounted, with nothing said. Now an error is "could not tell": the run says
# so, with the probe's own message, and unmounts anyway — umount's answer
# decides.
setup
sources hdd-media=/.btrfs-hdd
mount_sources
touch "$WORK/probe_errors/.btrfs-hdd"
unmount_all
check "probe cannot tell, helper mounted: unmounted anyway, once" \
    "$(calls /.btrfs-hdd) $(is_mounted /.btrfs-hdd)" "1 no"
check "probe cannot tell, helper mounted: says so, with the probe's message" \
    "$(logged 'WARN:   Could not tell whether source volume /.btrfs-hdd, which this run mounted, is still mounted — mountpoint: /.btrfs-hdd: Input/output error (exit 1); unmounting it anyway')" "1"
check "probe cannot tell, helper mounted: then unmounted" \
    "$(logged 'INFO:   Unmounted source volume /.btrfs-hdd (this run mounted it)')" "1"
rm -f "$WORK/probe_errors/.btrfs-hdd"
unmount_all
check "probe cannot tell, helper mounted: struck off once unmounted" "$(calls /.btrfs-hdd)" "1"

# Could not tell, and it was not mounted after all: umount says so, the run
# says that too, and the record keeps it until a probe can tell.
setup
sources hdd-media=/.btrfs-hdd
mount_sources
rm -f "$WORK/mounted/.btrfs-hdd"
touch "$WORK/probe_errors/.btrfs-hdd"
unmount_all
check "probe cannot tell, helper gone: unmount tried once" "$(calls /.btrfs-hdd)" "1"
check "probe cannot tell, helper gone: umount's own message, and no claim it is mounted" \
    "$(logged 'WARN:   Could not unmount source volume /.btrfs-hdd, which this run mounted: umount: /.btrfs-hdd: not mounted. — left as it is; best effort, not a DAS disconnect concern')" "1"
rm -f "$WORK/probe_errors/.btrfs-hdd"
unmount_all
check "probe cannot tell, helper gone: a later pass that can tell strikes it off, no umount" \
    "$(calls /.btrfs-hdd)" "1"
check "probe cannot tell, helper gone: and says why" \
    "$(logged 'INFO:   Source volume /.btrfs-hdd, recorded as this run'"'"'s, is not mounted now — nothing to unmount')" "1"

# Not mounted (32), or not there at all: struck off, said, never unmounted.
setup
sources hdd-media=/.btrfs-hdd ssd=/.btrfs-ssd
mount_sources
rm -f "$WORK/mounted/.btrfs-hdd" "$WORK/mounted/.btrfs-ssd"
touch "$WORK/absent/.btrfs-ssd"
unmount_all
check "probe says not mounted, or no such path: never unmounted" \
    "$(calls /.btrfs-hdd) $(calls /.btrfs-ssd)" "0 0"
check "probe says not mounted, or no such path: each said" \
    "$(grep -c "INFO:   Source volume /.btrfs-\(hdd\|ssd\), recorded as this run's, is not mounted now — nothing to unmount" "$WORK/log" || true)" "2"
check "probe says not mounted, or no such path: no warning" "$(grep -c '^WARN:' "$WORK/log" || true)" "0"
unmount_all
check "probe says not mounted, or no such path: struck off" "$(calls /.btrfs-hdd) $(calls /.btrfs-ssd)" "0 0"

# --- fstab declares a source mount point the run finds unmounted --------------
# Said once, as INFO: fstab's own mount is missing, and only this run's helper
# stands in for it while the run lasts. Not for one found mounted, nor for a
# path fstab does not declare.
setup
sources hdd-media=/.btrfs-hdd hdd-system=/.btrfs-hdd ssd=/.btrfs-ssd das-storage=/dasRaid0
touch "$WORK/fstab/.btrfs-hdd" "$WORK/fstab/dasRaid0"
premounted /dasRaid0
mount_sources
# Whichever of the two sources on /.btrfs-hdd bash's hash order visits first
# mounts it, and that is the one the line names.
mounted_by="$(sed -n 's|^INFO:   Mounted \(.*\) at /.btrfs-hdd$|\1|p' "$WORK/log")"
check "fstab declares it, not mounted: mounted by one of its two sources" \
    "$([[ "$mounted_by" == hdd-media || "$mounted_by" == hdd-system ]] && echo yes || echo "no ($mounted_by)")" "yes"
check "fstab declares it, not mounted: said, as INFO, for the source that mounts it" \
    "$(logged "INFO:   $mounted_by: fstab mounts /.btrfs-hdd at boot, but it was not mounted — this run mounts a helper there and takes it down at the end")" "1"
check "fstab declares it, not mounted: said once" "$(grep -c 'fstab mounts' "$WORK/log" || true)" "1"
check "fstab does not declare it, or it was found mounted: nothing said" \
    "$(grep -c 'fstab mounts /.btrfs-ssd\|fstab mounts /dasRaid0' "$WORK/log" || true)" "0"
unmount_all
check "fstab declares it: still the run's helper, taken down once; fstab's other mount untouched" \
    "$(calls /.btrfs-hdd) $(calls /dasRaid0)" "1 0"

# --- the probe's answer is a line it prints; an empty or unrecognised one is "could not tell" (bd hhow) ---
# bash returns 0 and an empty string for a command substitution it cannot make —
# no descriptor free for its pipe — so a probe read from `$(probe …) || rc=$?`
# had a fourth answer, silent, that read as "mounted": the unmount gate took a
# drive it could not ask about for one to unmount, and the boot-subvolume step
# went on to a target it had not looked at. probe_mount_point prints
# "mounted", "not-mounted" or "unknown: <why>", mountpoint's own status inside
# its capture, and probe_state reads the line: anything else is "unknown".
real_probe="$(declare -f probe_mount_point)"
state_of() { probe_state "$1"; echo "$PROBE_STATE|$PROBE_WHY"; }

# What the helper prints, for each way mountpoint can answer.
setup
touch "$WORK/probe_errors/err" "$WORK/absent/gone" "$WORK/mounted/mnt" "$WORK/probe_two/two" "$WORK/probe_fake/fake"
echo 2 >"$WORK/probe_rc/rc2"
echo 143 >"$WORK/probe_rc/killed"
check "probe prints 'mounted' for a mount point" "$(probe_mount_point /x/mnt)" "mounted"
check "probe prints 'not-mounted' for a path that is not one" "$(probe_mount_point /x/plain)" "not-mounted"
check "probe prints 'not-mounted' for a path that is not there" "$(probe_mount_point /x/gone)" "not-mounted"
check "probe prints 'unknown' with mountpoint's message and status for an error" \
    "$(probe_mount_point /x/err)" "unknown: mountpoint: /x/err: Input/output error (exit 1)"
check "probe joins a two-line message with '; '" \
    "$(probe_mount_point /x/two)" "unknown: mountpoint: first line; second line (exit 1)"
check "probe says so when mountpoint exits 2 and says nothing" \
    "$(probe_mount_point /x/rc2)" "unknown: mountpoint printed nothing (exit 2)"
check "probe says so when mountpoint is killed by TERM (143)" \
    "$(probe_mount_point /x/killed)" "unknown: mountpoint printed nothing (exit 143)"
check "probe is not fooled by a message that ends in 'status=0'" \
    "$(probe_mount_point /x/fake)" "unknown: status=0 (exit 1)"
# A capture that lost its status line is no answer, and is never "mounted".
# Only the capture's own failure can drop the line (printf, a builtin, cannot
# fail), so it is simulated: a printf that prints nothing.
lost="$( (printf() { :; }; probe_mount_point /x/mnt) )" || true
check "probe says so when its capture lost the status line, and does not call it mounted" \
    "$lost" "unknown: no answer — the probe's output could not be captured (no descriptor free for its pipe?)"
mkdir -p "$WORK/no-tools"
# shellcheck disable=SC2123  # the point: a PATH with no mountpoint on it
no_program="$( (unset -f mountpoint; PATH="$WORK/no-tools"; probe_mount_point /x/mnt) )" || true
check "probe says so when there is no mountpoint program (exit 127)" \
    "$([[ "$no_program" == "unknown: "*"mountpoint: command not found (exit 127)" ]] && echo yes || echo "no: $no_program")" "yes"

# How a caller reads it.
check "probe_state: a mount point" "$(state_of /x/mnt)" "mounted|"
check "probe_state: not one" "$(state_of /x/plain)" "not-mounted|"
check "probe_state: an error is unknown, with the reason" \
    "$(state_of /x/err)" "unknown|mountpoint: /x/err: Input/output error (exit 1)"
probe_mount_point() { :; }
check "probe_state, a probe that prints nothing: unknown, never mounted" \
    "$(state_of /x/mnt)" "unknown|the mount probe printed nothing (its capture failed?)"
probe_mount_point() { echo banana; }
check "probe_state, a probe that prints something else: unknown" \
    "$(state_of /x/mnt)" "unknown|the mount probe printed something unrecognised: banana"
probe_mount_point() { printf 'mounted\nand something more\n'; }
check "probe_state, an answer with more than the answer in it: unknown" \
    "$(state_of /x/mnt | head -n 1)" "unknown|the mount probe printed something unrecognised: mounted"
eval "$real_probe"

# The unmount gate, with a probe that says nothing: the same FAIL as for an
# error, naming both targets, the unmount tried anyway, never "safe".
setup
probe_mount_point() { :; }
unmount_all
check "gate, a probe that prints nothing: the unmount FAILS" "${OP_STATUS[unmount]:-<none>}" "FAIL"
check "gate, a probe that prints nothing: the detail names both targets and why" \
    "${OP_STATUS[unmount_detail]:-<none>}" \
    "could not tell whether /mnt/backup-system-recovery-A is mounted — the mount probe printed nothing (its capture failed?); umount then succeeded; could not tell whether /mnt/backup-22tb is mounted — the mount probe printed nothing (its capture failed?); umount then succeeded"
check "gate, a probe that prints nothing: each target is still unmounted, once" \
    "$(calls /mnt/backup-22tb) $(calls /mnt/backup-system-recovery-A) $(is_mounted /mnt/backup-22tb) $(is_mounted /mnt/backup-system-recovery-A)" "1 1 no no"
check "gate, a probe that prints nothing: said as an error, per target" \
    "$(grep -c '^ERROR:   Could not tell whether /mnt/backup-.* is mounted — the mount probe printed nothing' "$WORK/log" || true)" "2"
eval "$real_probe"

setup
probe_mount_point() { echo banana; }
unmount_all
check "gate, a probe that prints something unrecognised: the unmount FAILS, naming it" \
    "${OP_STATUS[unmount]:-<none>} $(grep -c 'could not tell whether /mnt/backup-22tb is mounted — the mount probe printed something unrecognised: banana; umount then succeeded' <<<"${OP_STATUS[unmount_detail]:-}" || true)" \
    "FAIL 1"
eval "$real_probe"

# The sources the run mounted: said, with the reason, and unmounted anyway.
setup
sources hdd-media=/.btrfs-hdd
mount_sources
probe_mount_point() { :; }
unmount_all
check "source, a probe that prints nothing: said, with why, and unmounted anyway" \
    "$(logged 'WARN:   Could not tell whether source volume /.btrfs-hdd, which this run mounted, is still mounted — the mount probe printed nothing (its capture failed?); unmounting it anyway')" "1"
check "source, a probe that prints nothing: unmounted once, and struck off" \
    "$(calls /.btrfs-hdd) $(is_mounted /.btrfs-hdd)" "1 no"
check "source, a probe that prints nothing: the run's own unmount is logged" \
    "$(logged 'INFO:   Unmounted source volume /.btrfs-hdd (this run mounted it)')" "1"
eval "$real_probe"

# Descriptor starvation. The limit is lowered in a subshell whose output is on
# a file already, and the stub mountpoint is a function that needs no
# descriptor (the file-backed one above captures basename's output). Whatever
# the limit, a probe is never read as the opposite of what it can say: one
# that can only say "not mounted" is never "mounted", one that can only say
# "mounted" is never "not mounted", and one that errs is always unknown. At 3
# nothing at all can be captured, so every answer is unknown; with the old
# reading each of those three was "mounted".
at_limit() { # at_limit <limit> <mounted|notmounted|error>: probe_state's answer
    local out="$WORK/limit.out"
    : >"$out"
    (
        MODE="$2"
        mountpoint() {
            case "$MODE" in
                mounted) return 0 ;;
                notmounted) return 32 ;;
                error)
                    echo "mountpoint: $1: Input/output error" >&2
                    return 1
                    ;;
            esac
        }
        ulimit -n "$1"
        probe_state /x/m
        printf '%s\n' "$PROBE_STATE"
    ) >"$out" 2>/dev/null
    cat "$out"
}
for limit in 3 4 5 6 7 8 9 10; do
    got_mounted="$(at_limit "$limit" mounted)"
    got_not="$(at_limit "$limit" notmounted)"
    got_error="$(at_limit "$limit" error)"
    verdict=ok
    [[ "$got_mounted" == mounted || "$got_mounted" == unknown ]] || verdict="a probe saying mounted read as '$got_mounted'"
    [[ "$got_not" == not-mounted || "$got_not" == unknown ]] || verdict="a probe saying not mounted read as '$got_not'"
    [[ "$got_error" == unknown ]] || verdict="a probe that errs read as '$got_error'"
    check "descriptor limit $limit: no probe is read as the opposite of what it says" "$verdict" "ok"
done
check "descriptor limit 3: nothing can be captured, so every answer is unknown" \
    "$(at_limit 3 mounted) $(at_limit 3 notmounted) $(at_limit 3 error)" "unknown unknown unknown"

# Nothing but probe_state may capture the probe: it is named in code twice
# only, by its definition and by probe_state's one call.
check "probe_mount_point is named in code only by its definition and by probe_state" \
    "$(grep -v '^[[:space:]]*#' "$SCRIPT" | grep -c 'probe_mount_point')" "2"

if [[ $fails -eq 0 ]]; then
    echo "UNMOUNT RETRY SUITE GREEN"
else
    echo "$fails FAILED"
    exit 1
fi
