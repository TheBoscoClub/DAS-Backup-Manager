#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: the variables below are read by the functions extracted from the
#   scripts, so their use is invisible here.
# SC2329: the log/sleep/mountpoint stubs are called by that code.
#
# tests/test_maintenance_lock.sh
#
# The shell side of the DAS maintenance lock (bd DAS-Backup-Manager-frb):
# acquire_maintenance_lock() and run_indexer() from scripts/backup-run.sh, and
# take_maintenance_lock() from scripts/backup-verify.sh, run against a lock
# file in a temp dir. Nothing here mounts anything; `btrdasd` is a stub.
#
# Whoever holds the lock records itself in the lock file, so a restore or
# index job that finds it held can say what it is waiting for:
#   - a backup that takes the lock records "backup-run.sh pid <pid>";
#   - a backup that is only WAITING leaves the holder's record alone — the
#     file is opened `<>`, never `>`, which would empty it on open;
#   - `btrdasd walk`, run by the backup while it holds the lock, is handed
#     that lock (fd 8, named in DAS_MAINTENANCE_LOCK_FD) — without it, walk
#     would wait for its own caller forever;
#   - backup-verify.sh records itself too, leaves another holder's record
#     alone, and skips its read-only mount while the lock is held.

# The shell options both scripts run under.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$ROOT/scripts/backup-run.sh"
VERIFY="$ROOT/scripts/backup-verify.sh"
WORK="$(mktemp -d)"
HOLDER_PID=""
cleanup() {
    : >"$WORK/release" 2>/dev/null || true
    [[ -n "$HOLDER_PID" ]] && wait "$HOLDER_PID" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

extract() { sed -n "/^$2() {/,/^}/p" "$1"; }
for fn in acquire_maintenance_lock record_maintenance_holder run_indexer record_op; do
    body="$(extract "$RUN" "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done
body="$(extract "$VERIFY" take_maintenance_lock)"
[[ -n "$body" ]] || { echo "FAIL: take_maintenance_lock not found in backup-verify.sh"; exit 1; }
eval "$body"

LOCK="$WORK/das-maintenance.lock"
MAINTENANCE_LOCKFILE="$LOCK"
declare -A OP_STATUS=() # written by record_op, as in backup-run.sh
log_info() { echo "INFO: $*" >>"$WORK/log"; }
log_warn() { echo "WARN: $*" >>"$WORK/log"; }
log_error() { echo "ERROR: $*" >>"$WORK/log"; }
sleep() { :; } # the 5 s announce probe; the waits below use `command sleep`
mountpoint() { true; }

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }
record() { head -n 1 "$LOCK" 2>/dev/null || true; }
free() { if flock -n "$LOCK" true; then echo free; else echo held; fi; }
wait_for_file() { local n=0; until [[ -e "$1" ]]; do command sleep 0.05; n=$((n + 1)); ((n < 200)) || return 1; done; }

# Another job takes the lock in its own process and records itself; it holds
# the lock until $WORK/release appears.
hold_lock() {
    rm -f "$WORK/release" "$WORK/held"
    bash -c 'exec 7<>"$1"; flock 7; printf "%s\n" "$2" >"$1"; : >"$3"; until [[ -e "$4" ]]; do sleep 0.05; done' \
        _ "$LOCK" "$1" "$WORK/held" "$WORK/release" &
    HOLDER_PID=$!
    wait_for_file "$WORK/held"
}
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

# --- a waiting backup leaves the holder's record alone -----------------------
rm -f "$LOCK"
: >"$WORK/log"
hold_lock "btrdasd scrub run pid 4242"
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
check "while waiting: the backup announced the wait" "$(grep -c 'waiting\.\.\.' "$WORK/log")" "1"
check "while waiting: the holder's record is intact" "$(record)" "btrdasd scrub run pid 4242"
check "while waiting: the backup has not taken the lock" "$([[ -e "$WORK/acquired" ]] && echo yes || echo no)" "no"
release_lock
wait_for_file "$WORK/acquired" || true
check "after release: the backup records itself" "$(cat "$WORK/record" 2>/dev/null)" "backup-run.sh pid $$"
: >"$WORK/done"
wait "$BACKUP_PID"

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

# --- backup-verify.sh --------------------------------------------------------
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

: >"$WORK/log"
hold_lock "btrdasd restore browse pid 77"
(
    if take_maintenance_lock "$LOCK"; then echo took >"$WORK/verify"; else echo skipped >"$WORK/verify"; fi
)
check "verify, held lock: skips the mount" "$(cat "$WORK/verify")" "skipped"
check "verify, held lock: says why" "$(grep -c 'maintenance lock held' "$WORK/log")" "1"
check "verify, held lock: the holder's record is intact" "$(record)" "btrdasd restore browse pid 77"
release_lock

if [[ $fails -eq 0 ]]; then
    echo "MAINTENANCE LOCK SUITE GREEN"
else
    echo "$fails FAILED"
    exit 1
fi
