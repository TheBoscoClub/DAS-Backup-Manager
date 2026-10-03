#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: the variables below are read by the functions extracted from the
#   scripts, so their use is invisible here.
# SC2329: the log/sleep/mountpoint/mount stubs are called by that code.
#
# tests/test_maintenance_lock.sh
#
# The shell side of the DAS maintenance lock (bd DAS-Backup-Manager-frb):
# acquire_maintenance_lock(), the record_/clear_maintenance_holder() pair,
# maintenance_holder(), run_indexer() and cleanup() from scripts/backup-run.sh,
# and take_maintenance_lock(), clear_maintenance_record() and
# check_btrbk_status() from scripts/backup-verify.sh, run against a lock file
# in a temp dir. Nothing here mounts anything: `btrdasd`, `mount`, `umount`,
# `btrbk` and `btrfs` are stubs.
#
# Whoever holds the lock records itself in the lock file, so a restore or
# index job that finds it held can say what it is waiting for:
#   - a backup that takes the lock records "backup-run.sh pid <pid>", and
#     empties the record again on its way out while it still holds the lock;
#   - a backup that is only WAITING leaves the holder's record alone — the
#     file is opened `<>`, never `>`, which would empty it on open — and says
#     who it waits for;
#   - `btrdasd walk`, run by the backup while it holds the lock, is handed
#     that lock (fd 8, named in DAS_MAINTENANCE_LOCK_FD) — without it, walk
#     would wait for its own caller forever;
#   - backup-verify.sh records itself too, empties the record on exit, leaves
#     another holder's record alone, and skips its read-only mount while the
#     lock is held — without reporting that skip as a failed mount (bd 0oi).

# The shell options both scripts run under.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$ROOT/scripts/backup-run.sh"
VERIFY="$ROOT/scripts/backup-verify.sh"
WORK="$(mktemp -d)"
HOLDER_PID=""
cleanup_test() {
    : >"$WORK/release" 2>/dev/null || true
    [[ -n "$HOLDER_PID" ]] && wait "$HOLDER_PID" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup_test EXIT

extract() { sed -n "/^$2() {/,/^}/p" "$1"; }
for fn in acquire_maintenance_lock record_maintenance_holder clear_maintenance_holder maintenance_holder run_indexer record_op cleanup; do
    body="$(extract "$RUN" "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done
for fn in take_maintenance_lock clear_maintenance_record check_btrbk_status; do
    body="$(extract "$VERIFY" "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-verify.sh"; exit 1; }
    eval "$body"
done

LOCK="$WORK/das-maintenance.lock"
MAINTENANCE_LOCKFILE="$LOCK"
declare -A OP_STATUS=() # written by record_op, as in backup-run.sh
log_info() { echo "INFO: $*" >>"$WORK/log"; }
log_warn() { echo "WARN: $*" >>"$WORK/log"; }
log_error() { echo "ERROR: $*" >>"$WORK/log"; }
log_header() { :; }
sleep() { :; } # the 5 s announce probe; the waits below use `command sleep`
mountpoint() { true; }

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }
record() { head -n 1 "$LOCK" 2>/dev/null || true; }
free() { if flock -n "$LOCK" true; then echo free; else echo held; fi; }
wait_for_file() { local n=0; until [[ -e "$1" ]]; do command sleep 0.05; n=$((n + 1)); ((n < 200)) || return 1; done; }

# Another job takes the lock in its own process and records itself with its
# own (live) pid; it holds the lock until $WORK/release appears.
hold_lock() {
    rm -f "$WORK/release" "$WORK/held"
    bash -c 'exec 7<>"$1"; flock 7; printf "%s pid %s\n" "$2" "$$" >"$1"; echo "$$" >"$3"; until [[ -e "$4" ]]; do sleep 0.05; done' \
        _ "$LOCK" "$1" "$WORK/held" "$WORK/release" &
    HOLDER_PID=$!
    wait_for_file "$WORK/held"
}
holder_record() { echo "$1 pid $(cat "$WORK/held")"; }
release_lock() { : >"$WORK/release"; wait "$HOLDER_PID"; HOLDER_PID=""; }

# --- a backup that takes a free lock records itself -------------------------
: >"$WORK/log"
(
    acquire_maintenance_lock
    record >"$WORK/record"
    free >"$WORK/state"
)
check "free lock: the backup records itself" "$(cat "$WORK/record")" "backup-run.sh pid $$"
check "free lock: it is really held" "$(cat "$WORK/state")" "held"

# --- a waiting backup leaves the holder's record alone, and names it --------
rm -f "$LOCK"
: >"$WORK/log"
hold_lock "btrdasd scrub run"
scrub="$(holder_record "btrdasd scrub run")"
rm -f "$WORK/acquired" "$WORK/done"
(
    acquire_maintenance_lock
    record >"$WORK/record"
    : >"$WORK/acquired"
    until [[ -e "$WORK/done" ]]; do command sleep 0.05; done
) &
BACKUP_PID=$!
# The waiting line is logged after the file is opened and before the wait.
n=0
until grep -q 'waiting' "$WORK/log"; do command sleep 0.05; n=$((n + 1)); ((n < 200)) || break; done
check "while waiting: the backup names who it waits for" \
    "$(grep -c "DAS maintenance lock held by $scrub — waiting\.\.\." "$WORK/log")" "1"
check "while waiting: the holder's record is intact" "$(record)" "$scrub"
check "while waiting: the backup has not taken the lock" "$([[ -e "$WORK/acquired" ]] && echo yes || echo no)" "no"
release_lock
wait_for_file "$WORK/acquired" || true
check "after release: the backup records itself" "$(cat "$WORK/record" 2>/dev/null)" "backup-run.sh pid $$"
: >"$WORK/done"
wait "$BACKUP_PID"

# --- what the backup reads from the record -----------------------------------
printf '' >"$LOCK"
check "holder: an empty record is unknown" "$(maintenance_holder)" "an unknown holder"
printf 'recovery-os VM session A\n' >"$LOCK"
check "holder: a record without a pid is only the last one" "$(maintenance_holder)" \
    "an unknown holder (last recorded: recovery-os VM session A)"
printf 'backup-run.sh pid 4194304999\n' >"$LOCK"
check "holder: a record whose process is gone is not the holder" "$(maintenance_holder)" \
    "an unknown holder (the last recorded holder, backup-run.sh pid 4194304999, is no longer running)"
printf 'btrdasd restore browse pid %s\n' "$$" >"$LOCK"
check "holder: a record whose process runs names it" "$(maintenance_holder)" "btrdasd restore browse pid $$"
rm -f "$LOCK"
check "holder: no lock file is unknown" "$(maintenance_holder)" "an unknown holder"

# --- the backup empties its record on the way out, while it holds the lock ---
(
    exec 8<>"$LOCK"
    flock 8
    printf 'backup-run.sh pid %s\n' "$$" >"$LOCK"
    CLEANUP_ARMED="true"; SCRIPT_COMPLETED="true"; DRYRUN_BTRBK_CONF=""
    cleanup
)
check "on exit: the backup's record is emptied" "$(wc -c <"$LOCK" | tr -d ' ')" "0"
printf 'btrdasd scrub run pid 77\n' >"$LOCK"
(
    CLEANUP_ARMED="false"; SCRIPT_COMPLETED="false"; DRYRUN_BTRBK_CONF=""
    cleanup
)
check "exit before the lock is ours: another holder's record is left alone" "$(record)" "btrdasd scrub run pid 77"

# --- walk is handed the lock the backup holds --------------------------------
rm -f "$LOCK"
cat >"$WORK/btrdasd" <<'STUB'
#!/bin/bash
# What `btrdasd walk` would find: the descriptor it is told about, and
# whether that descriptor is the lock file and holds the lock.
{
    echo "args=$*"
    echo "fd=${DAS_MAINTENANCE_LOCK_FD:-unset}"
    fd="${DAS_MAINTENANCE_LOCK_FD:-}"
    if [[ -n "$fd" && /proc/self/fd/$fd -ef "$LOCK" ]]; then echo "is-the-lock-file=yes"; else echo "is-the-lock-file=no"; fi
    if [[ -n "$fd" ]] && flock -n "$fd"; then echo "holds-it=yes"; else echo "holds-it=no"; fi
    if flock -n "$LOCK" true; then echo "a-fresh-open-gets-it=yes"; else echo "a-fresh-open-gets-it=no"; fi
} >"$OUT"
echo "Discovered: 0 snapshots"
STUB
chmod +x "$WORK/btrdasd"
BTRDASD_BIN="$WORK/btrdasd"
DAS_DB_PATH="$WORK/index.db"
declare -A TARGET_ROLES=([primary-22tb]=primary)
declare -A TARGET_MOUNTS=([primary-22tb]=/mnt/backup-22tb)
export LOCK
export OUT="$WORK/walk"
: >"$WORK/log"
(
    acquire_maintenance_lock
    run_indexer
    echo "${OP_STATUS[indexer]:-unset}" >"$WORK/indexer_status"
)
check "walk: run as before" "$(grep '^args=' "$WORK/walk")" "args=walk /mnt/backup-22tb --db $WORK/index.db"
check "walk: told which descriptor holds the lock" "$(grep '^fd=' "$WORK/walk")" "fd=8"
check "walk: that descriptor is the lock file" "$(grep '^is-the-lock-file=' "$WORK/walk")" "is-the-lock-file=yes"
check "walk: and holds the lock" "$(grep '^holds-it=' "$WORK/walk")" "holds-it=yes"
check "walk: while nobody else could take it" "$(grep '^a-fresh-open-gets-it=' "$WORK/walk")" "a-fresh-open-gets-it=no"
check "walk: the indexer step is recorded OK" "$(cat "$WORK/indexer_status")" "OK"

# --- backup-verify.sh takes the lock and records itself ---------------------
rm -f "$LOCK"
: >"$WORK/log"
(
    if take_maintenance_lock "$LOCK"; then echo took >"$WORK/verify"; else echo skipped >"$WORK/verify"; fi
    record >"$WORK/record"
    free >"$WORK/state"
)
check "verify, free lock: takes it" "$(cat "$WORK/verify")" "took"
check "verify, free lock: records itself" "$(cat "$WORK/record")" "backup-verify.sh pid $$"
check "verify, free lock: it is really held" "$(cat "$WORK/state")" "held"
check "verify, on exit: its record is emptied" "$(wc -c <"$LOCK" | tr -d ' ')" "0"

: >"$WORK/log"
hold_lock "btrdasd restore browse"
browse="$(holder_record "btrdasd restore browse")"
(
    if take_maintenance_lock "$LOCK"; then echo took >"$WORK/verify"; else echo skipped >"$WORK/verify"; fi
)
check "verify, held lock: skips the mount" "$(cat "$WORK/verify")" "skipped"
check "verify, held lock: says why" "$(grep -c 'maintenance lock held' "$WORK/log")" "1"
check "verify, held lock: the holder's record is intact" "$(record)" "$browse"
release_lock

# --- backup-verify.sh: a deliberate skip is not a failed mount (bd 0oi) ------
# check_btrbk_status only tries the primary when its partition is a block
# device; any node ending in 1 will do — `mount` is a stub, nothing is touched.
blockdev=""
for b in /dev/loop1 /dev/vda1 /dev/sda1 /dev/nvme0n1p1 /dev/ram1; do
    if [[ -b "$b" ]]; then blockdev="$b"; break; fi
done
if [[ -z "$blockdev" ]]; then
    echo "FAIL: no block device node ending in 1 to drive check_btrbk_status with"
    fails=$((fails + 1))
else
    DAS_DEVICES=("${blockdev%1}")
    DAS_BTRBK_CONF="$WORK/btrbk.conf"
    : >"$DAS_BTRBK_CONF"
    DAS_TARGET_COUNT=1
    DAS_TARGET_0_ROLE="primary"
    DAS_TARGET_0_SERIAL="SERIAL1"
    DAS_TARGET_0_MOUNT="$WORK/primary"
    read_drive_serial() { echo "SERIAL1"; }
    mount() { echo "mount $*" >>"$WORK/mounts"; echo "wrong fs type" >&2; return 32; }
    # Never reached while the mount fails; stubbed so nothing real can run.
    umount() { echo "umount $*" >>"$WORK/mounts"; }
    btrbk() { echo "btrbk $*" >>"$WORK/mounts"; }
    btrfs() { echo "btrfs $*" >>"$WORK/mounts"; }

    : >"$WORK/log"; : >"$WORK/mounts"
    take_maintenance_lock() { log_warn "DAS maintenance lock held by another job — skipping btrbk/usage inspection"; return 1; }
    check_btrbk_status
    check "verify skip: the skip is said" "$(grep -c 'skipping btrbk/usage inspection' "$WORK/log")" "1"
    check "verify skip: no failed mount is reported" "$(grep -c 'Failed to mount' "$WORK/log" || true)" "0"
    check "verify skip: nothing was mounted" "$(wc -l <"$WORK/mounts" | tr -d ' ')" "0"

    : >"$WORK/log"; : >"$WORK/mounts"
    take_maintenance_lock() { return 0; }
    check_btrbk_status
    check "verify, lock taken and the mount fails: that IS reported" \
        "$(grep -c "^ERROR: Failed to mount $blockdev at $WORK/primary: wrong fs type" "$WORK/log")" "1"
    check "verify, lock taken: the mount was tried" "$(wc -l <"$WORK/mounts" | tr -d ' ')" "1"
fi

if [[ $fails -eq 0 ]]; then
    echo "MAINTENANCE LOCK SUITE GREEN"
else
    echo "$fails FAILED"
    exit 1
fi
