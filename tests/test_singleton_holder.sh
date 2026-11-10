#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: LOCKFILE and SINGLETON_WAIT_SECS are read by the functions extracted
#   from the script, so their use is invisible here.
# SC2329: sleep is not stubbed; the functions are called from subshells.
#
# tests/test_singleton_holder.sh
#
# The singleton lock's holder record (bd DAS-Backup-Manager-al9):
# acquire_singleton_lock(), read_singleton_holder(), record_singleton_holder()
# and clear_singleton_holder() from scripts/backup-run.sh, run against a lock
# file in a temp dir. Nothing here mounts or runs anything real.
#
# Whoever holds /run/das-backup.lock records itself in it, as every holder of
# the maintenance lock does, so a run that finds it held can tell a backup
# (skip: redundant) from setup or a configuration edit (wait: a short hold
# that must not cost the day's backup), and a skip it cannot explain says so:
#   - a backup that takes the lock records "backup-run.sh pid <pid>";
#   - one that does not get it leaves the holder's record alone (opened `<>`,
#     never `>`, which would empty it on open);
#   - held by a backup: skip, exit 0, naming the holder;
#   - held by setup or a config edit: wait, saying who for, then go on; when
#     the wait runs out, skip, exit 0, saying the backup did NOT run;
#   - nothing recorded / no pid / the pid gone: skip, exit 0, a WARN that
#     the run did NOT happen — never a silent skip.

set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUN="${SINGLETON_SCRIPT:-$ROOT/scripts/backup-run.sh}"
WORK="$(mktemp -d)"
HOLDER_PID=""
cleanup_test() {
    : >"$WORK/release" 2>/dev/null || true
    : >"$WORK/done" 2>/dev/null || true
    [[ -n "$HOLDER_PID" ]] && wait "$HOLDER_PID" 2>/dev/null
    rm -rf "${WORK:?}"
}
trap cleanup_test EXIT

extract() { sed -n "/^$2() {/,/^}/p" "$1"; }
for fn in read_singleton_holder record_singleton_holder clear_singleton_holder acquire_singleton_lock; do
    body="$(extract "$RUN" "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in $RUN"; exit 1; }
    eval "$body"
done

LOCK="$WORK/das-backup.lock"
LOCKFILE="$LOCK"
SINGLETON_WAIT_SECS=30

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }
record() { head -n 1 "$LOCK" 2>/dev/null || true; }
free() { if flock -n "$LOCK" true; then echo free; else echo held; fi; }
wait_for_file() { local n=0; until [[ -e "$1" ]]; do command sleep 0.05; n=$((n + 1)); ((n < 400)) || return 1; done; }
has() { grep -c -F -- "$2" "$1" || true; }

# Another job takes the lock in its own process and records $1 plus its own
# (live) pid, or nothing when $1 is empty; it holds the lock until
# $WORK/release appears.
hold_lock() {
    rm -f "${WORK:?}/release" "${WORK:?}/held"
    bash -c 'exec 7<>"$1"; flock 7; if [[ -n "$2" ]]; then printf "%s pid %s\n" "$2" "$$" >"$1"; fi; echo "$$" >"$3"; until [[ -e "$4" ]]; do sleep 0.05; done' \
        _ "$LOCK" "$1" "$WORK/held" "$WORK/release" &
    HOLDER_PID=$!
    wait_for_file "$WORK/held"
}
holder_record() { echo "$1 pid $(cat "$WORK/held")"; }
release_lock() { : >"$WORK/release"; wait "$HOLDER_PID"; HOLDER_PID=""; }

# A run that acquires, notes what it read, and holds until $WORK/done appears.
start_run() {
    rm -f "${WORK:?}/reached" "${WORK:?}/rec" "${WORK:?}/done" "${WORK:?}/out" "${WORK:?}/rc"
    (
        rc=0
        (
            acquire_singleton_lock
            record >"$WORK/rec"
            : >"$WORK/reached"
            until [[ -e "$WORK/done" ]]; do command sleep 0.05; done
        ) >"$WORK/out" 2>&1 || rc=$?
        echo "$rc" >"$WORK/rc"
    ) &
    RUN_PID=$!
}
finish_run() { : >"$WORK/done"; wait "$RUN_PID"; }
reached() { [[ -e "$WORK/reached" ]] && echo yes || echo no; }

# --- a run that takes a free lock records itself, and the lock is root's alone
rm -f "${LOCK:?}"
: >"$LOCK"
chmod 644 "$LOCK"
(
    acquire_singleton_lock
    record >"$WORK/record"
    free >"$WORK/state"
    stat -c %a "$LOCK" >"$WORK/mode"
)
check "free lock: the run records itself" "$(cat "$WORK/record")" "backup-run.sh pid $$"
check "free lock: it is really held" "$(cat "$WORK/state")" "held"
check "free lock: a 0644 lock file is narrowed to 0600" "$(cat "$WORK/mode")" "600"

# --- clear_singleton_holder empties the record and keeps the lock ------------
(
    acquire_singleton_lock
    clear_singleton_holder
    record >"$WORK/record"
    free >"$WORK/state"
    [[ -f "$LOCK" ]] && echo present >"$WORK/file"
)
check "cleared: the record is empty" "$(cat "$WORK/record")" ""
check "cleared: the lock is still held" "$(cat "$WORK/state")" "held"
check "cleared: the file is never removed" "$(cat "$WORK/file")" "present"

# --- held by a backup: skip, exit 0, naming it, and leave its record alone ----
for who in "backup-run.sh" "btrdasd backup run" "btrdasd recovery-os record"; do
    rm -f "${LOCK:?}"
    hold_lock "$who"
    want="$(holder_record "$who")"
    start_run
    wait "$RUN_PID"
    check "$who holds it: exit status" "$(cat "$WORK/rc")" "0"
    check "$who holds it: the run did not go on" "$(reached)" "no"
    check "$who holds it: skip is said" "$(has "$WORK/out" "skipping this invocation")" "1"
    check "$who holds it: the holder is named" "$(has "$WORK/out" "($want)")" "1"
    check "$who holds it: its record is intact" "$(record)" "$want"
    release_lock
done

# --- held by setup or a config edit: wait, then go on -------------------------
for who in "btrdasd setup --upgrade" "btrdasd setup" "config edit"; do
    rm -f "${LOCK:?}"
    hold_lock "$who"
    want="$(holder_record "$who")"
    start_run
    wait_for_file "$WORK/out" || true
    n=0
    until [[ "$(has "$WORK/out" "waiting up to")" == 1 ]]; do command sleep 0.05; n=$((n + 1)); ((n < 400)) || break; done
    check "$who holds it: the wait is said, naming the holder" "$(has "$WORK/out" "held by $want — waiting up to 30s")" "1"
    command sleep 0.3
    check "$who holds it: the run has not gone on" "$(reached)" "no"
    check "$who holds it: its record is intact while waiting" "$(record)" "$want"
    release_lock
    wait_for_file "$WORK/reached" || true
    check "$who released it: the run went on" "$(reached)" "yes"
    check "$who released it: the run records itself" "$(cat "$WORK/rec" 2>/dev/null)" "backup-run.sh pid $$"
    finish_run
    check "$who released it: exit status" "$(cat "$WORK/rc")" "0"
done

# --- the wait runs out: skip, exit 0, and say the backup did NOT run ---------
rm -f "${LOCK:?}"
hold_lock "btrdasd setup"
want="$(holder_record "btrdasd setup")"
SINGLETON_WAIT_SECS=1
start_run
wait "$RUN_PID"
SINGLETON_WAIT_SECS=30
check "wait ran out: exit status" "$(cat "$WORK/rc")" "0"
check "wait ran out: the run did not go on" "$(reached)" "no"
check "wait ran out: says this backup did NOT run" "$(has "$WORK/out" "this backup did NOT run")" "1"
check "wait ran out: names who still holds it" "$(has "$WORK/out" "still held after 1s ($want)")" "1"
check "wait ran out: the holder's record is intact" "$(record)" "$want"
release_lock

# --- no usable record: skip, and never silently -------------------------------
rm -f "${LOCK:?}"
hold_lock ""
start_run
wait "$RUN_PID"
check "nothing recorded: exit status" "$(cat "$WORK/rc")" "0"
check "nothing recorded: the run did not go on" "$(reached)" "no"
check "nothing recorded: says the run did NOT run" "$(has "$WORK/out" "this one did NOT run")" "1"
check "nothing recorded: says nothing was recorded" "$(has "$WORK/out" "nothing recorded")" "1"
check "nothing recorded: the file is still empty (not truncated into a record)" "$(record)" ""
release_lock

rm -f "${LOCK:?}"
hold_lock "no-pid-here"
printf 'btrdasd setup\n' >"$LOCK" # a record with no pid
start_run
wait "$RUN_PID"
check "no pid recorded: exit status" "$(cat "$WORK/rc")" "0"
check "no pid recorded: says the run did NOT run" "$(has "$WORK/out" "this one did NOT run")" "1"
check "no pid recorded: is not waited for" "$(has "$WORK/out" "waiting up to")" "0"
release_lock

rm -f "${LOCK:?}"
hold_lock ""
gone="$(bash -c 'echo $$')" # a pid whose process has already exited
printf 'btrdasd setup pid %s\n' "$gone" >"$LOCK"
start_run
wait "$RUN_PID"
check "recorded pid gone: exit status" "$(cat "$WORK/rc")" "0"
check "recorded pid gone: says the run did NOT run" "$(has "$WORK/out" "this one did NOT run")" "1"
check "recorded pid gone: says the holder is no longer running" "$(has "$WORK/out" "is no longer running")" "1"
check "recorded pid gone: is not waited for" "$(has "$WORK/out" "waiting up to")" "0"
release_lock

# --- the holder lets go between this run's flock and its read of the record --
# The record reads empty then, and the lock is already free: one more try
# takes it, never a skip of a backup that could run (al9 review).
rm -f "${LOCK:?}"
hold_lock ""
rc=0
(
    eval "real_$(declare -f read_singleton_holder)"
    read_singleton_holder() {
        : >"$WORK/release"
        until flock -n "$LOCK" true; do command sleep 0.05; done
        real_read_singleton_holder
    }
    acquire_singleton_lock
    record >"$WORK/rec"
) >"$WORK/out" 2>&1 || rc=$?
wait "$HOLDER_PID"
HOLDER_PID=""
check "released during the read: exit status" "$rc" "0"
check "released during the read: the run takes the lock and records itself" "$(cat "$WORK/rec" 2>/dev/null)" "backup-run.sh pid $$"
check "released during the read: no 'did NOT run' line" "$(has "$WORK/out" "did NOT run")" "0"

# --- a lock file that cannot be opened is "could not start", exit 1 ----------
rm -rf "${LOCK:?}"
mkdir "$LOCK"
start_run
wait "$RUN_PID"
check "unopenable: exit status" "$(cat "$WORK/rc")" "1"
check "unopenable: says why" "$(has "$WORK/out" "Cannot open the backup lock $LOCK — could not start")" "1"
rmdir "$LOCK"

echo
if ((fails == 0)); then
    echo "SINGLETON HOLDER SUITE GREEN"
else
    echo "$fails FAILED"
    exit 1
fi
