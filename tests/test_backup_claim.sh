#!/bin/bash
# shellcheck disable=SC2016,SC2034,SC2329
# SC2034: the variables below are read by the functions extracted from the
#   script, so their use is invisible here.
# SC2016: wait_for takes its condition as a string, expanded when it runs.
# SC2329: the log/sleep stubs are called by that code.
#
# tests/test_backup_claim.sh
#
# The backup's side of the scrub yield (bd DAS-Backup-Manager-su1b.2, step
# A6): acquire_maintenance_lock() in scripts/backup-run.sh claims the
# maintenance lock while it waits for it — /run/das-maintenance-claim.lock
# held shared on fd 7 — so a scrub pass holding the lock gives it up, and
# lets the claim go the moment it holds the lock. Run against lock files in a
# temp dir, with a stand-in scrub in its own process:
#   - a scrub that watches the claim yields: the backup gets the lock, the
#     report row says the scrub yielded, the claim is free again, and no
#     child started afterwards carries the claim's descriptor;
#   - a holder that never answers: one WARN after YIELD_ANSWER_SECS naming
#     it, the backup keeps waiting, and the run's exit status is unchanged;
#   - no child the backup starts while it waits carries the claim, so a
#     backup killed by SIGKILL while it waits leaves no claim behind;
#   - a free lock, or one held by a job that is not a scrub, leaves no claim
#     and no yield in the report.

set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$ROOT/scripts/backup-run.sh"
WORK="$(mktemp -d)"
STARTED=()
cleanup_test() {
    : >"$WORK/release" 2>/dev/null || true
    local pid
    for pid in "${STARTED[@]}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "${WORK:?}"
}
trap cleanup_test EXIT

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }
has() { if [[ "$2" == *"$3"* ]]; then echo "ok   $1"; else echo "FAIL $1: '$2' lacks '$3'"; fails=$((fails + 1)); fi; }

extract() { sed -n "/^$2() {/,/^}/p" "$1"; }
for fn in acquire_maintenance_lock acquire_claim release_claim record_maintenance_holder maintenance_holder record_op monotonic_secs any_op_is completed_exit_status; do
    body="$(extract "$RUN" "$fn")"
    if [[ -n "$body" ]]; then
        eval "$body"
    else
        echo "FAIL $fn not found in backup-run.sh"
        fails=$((fails + 1))
    fi
done

LOCK="$WORK/das-maintenance.lock"
MAINTENANCE_LOCKFILE="$LOCK"
CLAIM_LOCKFILE="$WORK/das-maintenance-claim.lock"
YIELD_ANSWER_SECS=180
declare -A OP_STATUS=()
log_info() { echo "INFO: $*" >>"$WORK/log"; }
log_warn() { echo "WARN: $*" >>"$WORK/log"; }
log_error() { echo "ERROR: $*" >>"$WORK/log"; }
sleep() { :; } # the 5 s announce probe; the waits below use `command sleep`

wait_for() { local n=0; until eval "$1"; do command sleep 0.05; n=$((n + 1)); ((n < 400)) || return 1; done; }
# Whether a backup claims the lock: an exclusive probe is refused while a
# shared claim is held, as the scrub's probe is (maintenance::claim_is_held).
claim_state() { if flock -n -x "$CLAIM_LOCKFILE" true 2>/dev/null; then echo free; else echo claimed; fi; }
# How many of the descriptors of process $1 are on the claim file.
claim_fds() { find "/proc/$1/fd" -maxdepth 1 -lname "$CLAIM_LOCKFILE" 2>/dev/null | wc -l; }

# A stand-in holder of the maintenance lock in its own process, recorded as
# $1 with its own pid. "yield": it watches the claim, and once a backup claims
# it, it empties its record and lets go, as a scrub pass does. "deaf": it
# holds until $WORK/release appears.
holder() {
    rm -f "${WORK:?}/held" "${WORK:?}/release" "${WORK:?}/yielded"
    bash -c '
        exec 8<>"$1"; flock 8; printf "%s pid %s\n" "$2" "$$" >"$1"; echo "$$" >"$4"
        if [[ $3 == yield ]]; then
            n=0; while flock -n -x "$6" true; do sleep 0.02; n=$((n + 1)); ((n < 500)) || exit 0; done
            : >"$1"; flock -u 8; exec 8<&-; : >"$7"
        else
            until [[ -e "$5" ]]; do sleep 0.05; done
        fi' _ "$LOCK" "$1" "$2" "$WORK/held" "$WORK/release" "$CLAIM_LOCKFILE" "$WORK/yielded" &
    STARTED+=("$!")
    HOLDER_PID=$!
    wait_for '[[ -s "$WORK/held" ]]'
}
holder_record() { echo "$1 pid $(cat "$WORK/held")"; }

# --- a scrub that answers the claim yields; the claim goes once the lock is held
: >"$WORK/log"
OP_STATUS=()
holder "btrdasd scrub run" yield
scrub="$(holder_record "btrdasd scrub run")"
(
    acquire_maintenance_lock
    claim_state >"$WORK/after"
    printf '%s|%s' "${OP_STATUS[scrub_yield]:-}" "${OP_STATUS[scrub_yield_detail]:-}" >"$WORK/row"
    command sleep 30 &
    child=$!
    claim_fds "$child" >"$WORK/child_fds"
    kill "$child"
)
wait "$HOLDER_PID" || true
check "yield: the scrub saw the claim and let go" "$([[ -e "$WORK/yielded" ]] && echo yes || echo no)" "yes"
check "yield: the claim is let go once the lock is held" "$(cat "$WORK/after")" "free"
check "yield: a child started afterwards has no claim descriptor" "$(cat "$WORK/child_fds")" "0"
row="$(cat "$WORK/row")"
check "yield: the report row is OK" "${row%%|*}" "OK"
has "yield: the report row says the scrub yielded" "$row" "scrub yielded after"
has "yield: the log says the scrub was asked" "$(cat "$WORK/log")" "asked the scrub to yield"
has "yield: the log names who held the lock" "$(cat "$WORK/log")" "DAS maintenance lock held by $scrub — waiting..."
check "yield: the claim record is emptied" "$(cat "$CLAIM_LOCKFILE")" ""

# --- a holder that never answers: one WARN, the wait goes on, exit status 0 -
: >"$WORK/log"
OP_STATUS=()
YIELD_ANSWER_SECS=2
holder "btrdasd scrub run" deaf
deaf="$(holder_record "btrdasd scrub run")"
(
    acquire_maintenance_lock
    printf '%s|%s' "${OP_STATUS[scrub_yield]:-}" "${OP_STATUS[scrub_yield_detail]:-}" >"$WORK/row"
    completed_exit_status >"$WORK/exit"
) &
BACKUP_PID=$!
STARTED+=("$BACKUP_PID")
wait_for '[[ $(claim_state) == claimed ]]' || true
check "deaf: the waiting backup claims" "$(claim_state)" "claimed"
check "deaf: the claim names the backup" "$(cat "$CLAIM_LOCKFILE")" "backup-run.sh pid $$"
# Every child the backup runs while it waits (the blocked flock) has fd 7 shut.
wait_for '[[ -n $(pgrep -P "$BACKUP_PID") ]]' || true
kids_with_claim=0
for kid in $(pgrep -P "$BACKUP_PID"); do
    kids_with_claim=$((kids_with_claim + $(claim_fds "$kid")))
done
check "deaf: no child of the waiting backup holds the claim" "$kids_with_claim" "0"
wait_for 'grep -q "^WARN" "$WORK/log"' || true
warn="$(grep "^WARN" "$WORK/log" || true)"
has "deaf: the warning names the holder" "$warn" "still held by $deaf"
has "deaf: the warning says the backup keeps waiting" "$warn" "keeps waiting"
check "deaf: still waiting after the warning" "$(kill -0 "$BACKUP_PID" 2>/dev/null && echo waiting || echo gone)" "waiting"
: >"$WORK/release"
wait "$BACKUP_PID"
wait "$HOLDER_PID" || true
check "deaf: exactly one warning" "$(grep -c "^WARN" "$WORK/log")" "1"
row="$(cat "$WORK/row")"
check "deaf: the report row is WARN" "${row%%|*}" "WARN"
has "deaf: the row names the interval" "$row" "no answer after 2s"
check "deaf: the exit status is unchanged by the warning" "$(cat "$WORK/exit")" "0"
check "deaf: the claim is free once the backup holds the lock" "$(claim_state)" "free"
YIELD_ANSWER_SECS=180

# --- a backup SIGKILLed while it waits leaves no claim behind ----------------
: >"$WORK/log"
holder "btrdasd scrub run" deaf
(acquire_maintenance_lock) &
BACKUP_PID=$!
STARTED+=("$BACKUP_PID")
wait_for '[[ $(claim_state) == claimed ]]' || true
wait_for '[[ -n $(pgrep -P "$BACKUP_PID") ]]' || true
kids="$(pgrep -P "$BACKUP_PID" || true)"
kill -KILL "$BACKUP_PID"
wait "$BACKUP_PID" 2>/dev/null || true
wait_for '[[ $(claim_state) == free ]]' || true
check "SIGKILL: the claim is gone with the backup, its children still waiting" "$(claim_state)" "free"
for kid in $kids; do kill "$kid" 2>/dev/null || true; done
: >"$WORK/release"
wait "$HOLDER_PID" || true

# --- a free lock: no claim left, no yield reported ---------------------------
: >"$WORK/log"
OP_STATUS=()
(
    acquire_maintenance_lock
    claim_state >"$WORK/after"
    printf '%s|%s' "${OP_STATUS[scrub_yield]:-}" "${OP_STATUS[scrub_yield_detail]:-}" >"$WORK/row"
)
check "free: no claim is left" "$(cat "$WORK/after")" "free"
check "free: no yield is reported" "$(cat "$WORK/row")" "|"

# --- held by a job that is not a scrub: no yield is reported -----------------
: >"$WORK/log"
OP_STATUS=()
holder "btrdasd doctor" deaf
(
    acquire_maintenance_lock
    printf '%s|%s' "${OP_STATUS[scrub_yield]:-}" "${OP_STATUS[scrub_yield_detail]:-}" >"$WORK/row"
) &
BACKUP_PID=$!
STARTED+=("$BACKUP_PID")
wait_for '[[ $(claim_state) == claimed ]]' || true
: >"$WORK/release"
wait "$BACKUP_PID"
wait "$HOLDER_PID" || true
row="$(cat "$WORK/row")"
check "not a scrub: the row is N/A" "${row%%|*}" "N/A"
has "not a scrub: the row names the holder" "$row" "btrdasd doctor pid"
check "not a scrub: the log does not say a scrub was asked" "$(grep -c "asked the scrub to yield" "$WORK/log" || true)" "0"

if ((fails > 0)); then
    echo "test_backup_claim: $fails FAILED"
    exit 1
fi
echo "test_backup_claim: all passed"
